mod config;
mod errors;

// Removed BackendGroup from here as it's not directly used in main.rs
use config::{CliArgs, Strategy, BackendGroups, load_config, process_config};
use errors::{ProxyError, error_to_response};

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::convert::Infallible;

use clap::Parser;
use hyper::server::conn::AddrStream;
use hyper::service::{make_service_fn, service_fn};
// Removed unused Method import
use hyper::{Body, Request, Response, Server, Client, Uri};
use hyper::header::HeaderValue; // Added for CORS
use hyper_tls::HttpsConnector;
use dashmap::DashMap;
use rand::Rng; // Rng trait is sufficient for gen_range
use url::Url;
// Removed unused futures_util::future::FutureExt

use eyre::{Result, bail};

#[derive(Clone)]
struct AppState {
    backend_groups: Arc<BackendGroups>,
    strategy: Strategy,
    sticky_ip_map: Arc<DashMap<IpAddr, HashMap<String, usize>>>,
    client: Client<HttpsConnector<hyper::client::HttpConnector>>,
}

async fn handle_request(
    req: Request<Body>, // Removed `mut`
    state: AppState,
    remote_addr: SocketAddr,
) -> Result<Response<Body>, ProxyError> {
    // Clone essential parts of the request first.
    let method = req.method().clone();
    let uri = req.uri().clone(); // Cloned hyper::Uri
    let headers = req.headers().clone();

    tracing::info!("Incoming request: {} {} from {}", method, uri, remote_addr);

    // Extract path and prefix from the cloned URI's path.
    // path_str is owned, its slices ('prefix_slice', 'downstream_path_slice') are valid.
    let path_str = uri.path().to_string();
    let (prefix_slice, downstream_path_slice) = extract_prefix_and_downstream_path(&path_str)?;

    // Create an owned String for the prefix, as it's used as a key and in errors.
    let prefix_string = prefix_slice.to_string();

    let backend_group = state.backend_groups.get(&prefix_string)
        .ok_or_else(|| ProxyError::NoBackendsForPrefix(prefix_string.clone()))?;

    if backend_group.endpoints.is_empty() {
        return Err(ProxyError::NoBackendsForPrefix(prefix_string.clone()));
    }

    // Now, consume the original request's body. `req` is moved here.
    let body_bytes = hyper::body::to_bytes(req.into_body()).await
        .map_err(|_| ProxyError::RequestBodyTooLarge)?;

    let client_ip = remote_addr.ip();
    let num_backends = backend_group.endpoints.len();
    let mut attempt_order: Vec<usize> = (0..num_backends).collect();

    let initial_backend_idx = match state.strategy {
        Strategy::RoundRobin => {
            // Assumes BackendGroup.rr_counter is Arc<AtomicUsize> (fixed in config.rs)
            let count = backend_group.rr_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            count % num_backends
        }
        Strategy::StickyIp => {
            let mut ip_map_entry = state.sticky_ip_map.entry(client_ip).or_default();
            // The key in sticky_ip_map is String
            if let Some(idx) = ip_map_entry.get(&prefix_string) {
                *idx
            } else {
                let idx = rand::rng().random_range(0..num_backends);
                ip_map_entry.insert(prefix_string.clone(), idx); // Clone prefix_string for insertion
                idx
            }
        }
    };

    attempt_order.rotate_left(initial_backend_idx);

    // Use cloned method, uri (for query), headers for building backend request
    for backend_idx in attempt_order.iter() {
        let backend_base_url = &backend_group.endpoints[*backend_idx];
        // Pass downstream_path_slice and query from cloned uri
        let target_uri_hyper = build_target_uri(backend_base_url, downstream_path_slice, uri.query())?;

        tracing::debug!("Attempting to proxy to backend #{}: {}", backend_idx, target_uri_hyper);

        let mut backend_req_builder = Request::builder()
            .method(method.clone()) // Use cloned method
            .uri(target_uri_hyper.clone()); // Clone the target_uri for the request

        *backend_req_builder.headers_mut().unwrap() = headers.clone(); // Use cloned headers
        backend_req_builder.headers_mut().unwrap().remove(hyper::header::HOST);

        let backend_req = backend_req_builder
            .body(Body::from(body_bytes.clone()))
            .expect("Failed to build backend request");

        match state.client.request(backend_req).await {
            Ok(response) => {
                if response.status().is_success() || response.status().is_redirection() || response.status().is_client_error() {
                    tracing::info!("Successfully proxied to {} - Status: {}", target_uri_hyper, response.status());
                    let (mut parts, body) = response.into_parts();
                    parts.headers.insert("Access-Control-Allow-Origin", HeaderValue::from_static("*"));
                    return Ok(Response::from_parts(parts, body));
                } else {
                    tracing::warn!(
                        "Backend {} returned error status: {}. Trying next if available.",
                        target_uri_hyper,
                        response.status()
                    );
                }
            }
            Err(e) => {
                tracing::warn!("Failed to connect to backend {}: {}. Trying next if available.", target_uri_hyper, e);
            }
        }
    }

    Err(ProxyError::AllBackendsFailed{ prefix: prefix_string, attempts: num_backends }) // Use owned prefix_string
}


fn extract_prefix_and_downstream_path(path: &str) -> Result<(&str, &str), ProxyError> {
    if !path.starts_with('/') {
        return Err(ProxyError::InvalidPath);
    }
    let path_trimmed = &path[1..];
    match path_trimmed.split_once('/') {
        Some((prefix, rest)) => Ok((prefix, rest)),
        None => {
            if path_trimmed.is_empty() {
                Err(ProxyError::InvalidPath)
            } else {
                Ok((path_trimmed, ""))
            }
        }
    }
}

fn build_target_uri(backend_base_url: &Url, downstream_path: &str, query: Option<&str>) -> Result<Uri, ProxyError> {
    let mut target_url = backend_base_url.clone();
    if !downstream_path.is_empty() {
        let base_path = target_url.path().trim_end_matches('/');
        let downstream_path_trimmed = downstream_path.trim_start_matches('/');
        target_url.set_path(&format!("{}/{}", base_path, downstream_path_trimmed));
    }
    target_url.set_query(query);
    Uri::try_from(target_url.as_str()).map_err(ProxyError::UriParse)
}


#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli_args = CliArgs::parse();
    tracing::info!("Starting proxy with CLI args: {:?}", cli_args);

    let raw_config = load_config(&cli_args.config)
        .map_err(|e| {
            tracing::error!("Failed to load config: {}", e);
            e
        })?;

    let backend_groups_map = process_config(raw_config)
        .map_err(|e| {
            tracing::error!("Failed to process config: {}", e);
            e
        })?;
    let backend_groups = Arc::new(backend_groups_map);

    if backend_groups.is_empty() {
        tracing::error!("No backend groups loaded from config. Exiting.");
        bail!("No backends configured");
    }

    tracing::info!("Loaded {} backend groups: {:?}", backend_groups.len(), backend_groups.keys());
    // Reverted to iterating over values for this log, it's cleaner.
    for group in backend_groups.values() {
        tracing::info!(" - Prefix '{}': {} endpoints", group.prefix, group.endpoints.len());
    }

    let https = HttpsConnector::new();
    let client = Client::builder().build::<_, Body>(https);

    let app_state = AppState {
        backend_groups,
        strategy: cli_args.strategy,
        sticky_ip_map: Arc::new(DashMap::new()),
        client,
    };

    let make_svc = make_service_fn(move |conn: &AddrStream| {
        let state = app_state.clone();
        let remote_addr = conn.remote_addr();
        async move {
            Ok::<_, Infallible>(service_fn(move |req_hyper| { // Renamed req to req_hyper to avoid confusion
                let state = state.clone();
                async move {
                    match handle_request(req_hyper, state, remote_addr).await {
                        Ok(response) => Ok::<_, Infallible>(response), // Annotated Ok
                        Err(proxy_error) => {
                            tracing::error!("Error processing request from {}: {}", remote_addr.ip(), proxy_error);
                            Ok::<_, Infallible>(error_to_response(&proxy_error)) // Annotated Ok
                        }
                    }
                }
            }))
        }
    });

    let server = Server::bind(&cli_args.listen_address).serve(make_svc);
    tracing::info!("Proxy server listening on http://{}", cli_args.listen_address);

    server.await.map_err(|e| {
        tracing::error!("Server error: {}", e);
        e // Convert hyper::Error into eyre::Report
    })?;

    Ok(())
}


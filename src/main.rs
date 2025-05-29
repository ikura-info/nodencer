mod config;
mod errors;

use config::{CliArgs, Strategy, BackendGroups, load_config, process_config};
use errors::{ProxyError, error_to_response};

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::convert::Infallible;

use clap::Parser;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::server::conn::auto::Builder as ServerBuilder;
use hyper::body::{Body as HttpBody, Incoming as IncomingBody};
use http_body_util::{BodyExt, Full};
use bytes::Bytes;
use hyper::{Request, Response, Uri};
use hyper::header::HeaderValue;
use hyper_tls::HttpsConnector;
use dashmap::DashMap;
use rand::Rng;
use url::Url;

use eyre::{Result, bail};

#[derive(Clone)]
struct AppState {
    backend_groups: Arc<BackendGroups>,
    strategy: Strategy,
    sticky_ip_map: Arc<DashMap<IpAddr, HashMap<String, usize>>>,
    client: Client<HttpsConnector<HttpConnector>, Full<Bytes>>
}

async fn handle_request(
    req: Request<IncomingBody>,
    state: AppState,
    remote_addr: SocketAddr,
) -> Result<Response<Full<Bytes>>, ProxyError> {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();

    tracing::info!("Incoming request: {} {} from {}", method, uri, remote_addr);

    let path_str = uri.path().to_string();
    let (prefix_slice, downstream_path_slice) = extract_prefix_and_downstream_path(&path_str)?;
    let prefix_string = prefix_slice.to_string();

    let backend_group = state.backend_groups.get(&prefix_string)
        .ok_or_else(|| ProxyError::NoBackendsForPrefix(prefix_string.clone()))?;

    if backend_group.endpoints.is_empty() {
        return Err(ProxyError::NoBackendsForPrefix(prefix_string.clone()));
    }

    let body_bytes = req.into_body().collect().await.map_err(|e| ProxyError::Hyper(e))?.to_bytes();

    let client_ip = remote_addr.ip();
    let num_backends = backend_group.endpoints.len();
    let mut attempt_order: Vec<usize> = (0..num_backends).collect();

    let initial_backend_idx = match state.strategy {
        Strategy::RoundRobin => {
            let count = backend_group.rr_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            count % num_backends
        }
        Strategy::StickyIp => {
            let mut ip_map_entry = state.sticky_ip_map.entry(client_ip).or_default();
            if let Some(idx) = ip_map_entry.get(&prefix_string) {
                *idx
            } else {
                let idx = rand::thread_rng().gen_range(0..num_backends);
                ip_map_entry.insert(prefix_string.clone(), idx);
                idx
            }
        }
    };

    attempt_order.rotate_left(initial_backend_idx);

    for backend_idx in attempt_order.iter() {
        let backend_base_url = &backend_group.endpoints[*backend_idx];
        let target_uri_hyper = build_target_uri(backend_base_url, downstream_path_slice, uri.query())?;

        tracing::debug!("Attempting to proxy to backend #{}: {}", backend_idx, target_uri_hyper);

        let mut backend_req_builder = Request::builder()
            .method(method.clone())
            .uri(target_uri_hyper.clone());

        *backend_req_builder.headers_mut().unwrap() = headers.clone();

        let backend_req = backend_req_builder
            .body(Full::new(body_bytes.clone()))
            .expect("Failed to build backend request");

        match state.client.request(backend_req).await {
            Ok(response) => {
                if response.status().is_success() || response.status().is_redirection() || response.status().is_client_error() {
                    tracing::info!("Successfully proxied to {} - Status: {}", target_uri_hyper, response.status());
                    let (mut parts, body) = response.into_parts();
                    let body_bytes = body.collect().await.map_err(|e| ProxyError::Hyper(e))?.to_bytes();
                    parts.headers.insert("Access-Control-Allow-Origin", HeaderValue::from_static("*"));
                    return Ok(Response::from_parts(parts, Full::new(body_bytes)));
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

    Err(ProxyError::AllBackendsFailed{ prefix: prefix_string, attempts: num_backends })
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
    for group in backend_groups.values() {
        tracing::info!(" - Prefix '{}': {} endpoints", group.prefix, group.endpoints.len());
    }

    let https = HttpsConnector::new();
    let client = Client::builder(TokioExecutor::new()).build(https);

    let app_state = AppState {
        backend_groups,
        strategy: cli_args.strategy,
        sticky_ip_map: Arc::new(DashMap::new()),
        client,
    };

    let listener = tokio::net::TcpListener::bind(cli_args.listen_address).await?;
    tracing::info!("Proxy server listening on http://{}", cli_args.listen_address);

    loop {
        let (stream, remote_addr) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let state_clone = app_state.clone();

        tokio::task::spawn(async move {
            let service = hyper::service::service_fn(move |req: Request<IncomingBody>| {
                let state = state_clone.clone();
                async move {
                    match handle_request(req, state, remote_addr).await {
                        Ok(response) => Ok::<_, Infallible>(response),
                        Err(proxy_error) => {
                            tracing::error!("Error processing request from {}: {}", remote_addr.ip(), proxy_error);
                            Ok::<_, Infallible>(error_to_response(&proxy_error))
                        }
                    }
                }
            });

            if let Err(err) = ServerBuilder::new(TokioExecutor::new())
                .serve_connection(io, service)
                .await
            {
                tracing::error!("Error serving connection: {}", err);
            }
        });
    }
}


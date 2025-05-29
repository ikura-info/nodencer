use clap::Parser;
use serde::Deserialize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use url::Url; // Use url::Url for validated backend URLs

#[derive(Parser, Debug, Clone)]
#[clap(author, version, about, long_about = None)]
pub struct CliArgs {
    #[clap(short, long, value_parser)]
    pub config: PathBuf,

    #[clap(short, long, value_enum, default_value_t = Strategy::RoundRobin)]
    pub strategy: Strategy,

    #[clap(short, long, value_parser, default_value = "127.0.0.1:3000")]
    pub listen_address: SocketAddr,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum, PartialEq)]
pub enum Strategy {
    RoundRobin,
    StickyIp,
}

#[derive(Debug, Deserialize)]
#[serde(transparent)] // This means AppConfig is just a HashMap<String, Vec<String>>
pub struct RawAppConfig(pub HashMap<String, Vec<String>>);

// This will be the processed configuration
#[derive(Debug, Clone)]
pub struct BackendGroup {
    pub prefix: String,
    pub endpoints: Vec<Url>, // Store parsed Urls
    // For RoundRobin: counter for which endpoint to use next
    pub rr_counter: Arc<AtomicUsize>, // Changed from AtomicUsize
}

impl BackendGroup {
    pub fn new(prefix: String, endpoint_strings: Vec<String>) -> Result<Self, url::ParseError> {
        let endpoints = endpoint_strings
            .into_iter()
            .map(|s| Url::parse(&s))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            prefix,
            endpoints,
            rr_counter: Arc::new(AtomicUsize::new(0)),
        })
    }
}

// Key: prefix (e.g., "mainnet"), Value: The group of backends for that prefix
pub type BackendGroups = HashMap<String, std::sync::Arc<BackendGroup>>;

pub fn load_config(path: &PathBuf) -> Result<RawAppConfig, super::errors::ProxyError> {
    let file_content = std::fs::read_to_string(path)?;
    let raw_config: RawAppConfig = serde_yaml::from_str(&file_content)?;
    Ok(raw_config)
}

pub fn process_config(raw_config: RawAppConfig) -> Result<BackendGroups, super::errors::ProxyError> {
    let mut backend_groups = BackendGroups::new();
    for (prefix, endpoints_str) in raw_config.0 {
        if endpoints_str.is_empty() {
            tracing::warn!("Prefix '{}' has no endpoints defined, skipping.", prefix);
            continue;
        }
        let group = BackendGroup::new(prefix.clone(), endpoints_str)?;
        backend_groups.insert(prefix, std::sync::Arc::new(group));
    }
    Ok(backend_groups)
}
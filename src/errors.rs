use std::fmt;
use hyper::{StatusCode, Response};
use http_body_util::Full;
use bytes::Bytes;

#[derive(Debug)]
#[allow(dead_code)]
pub enum ProxyError {
    Io(std::io::Error),
    ConfigLoad(serde_yaml::Error),
    Hyper(hyper::Error),
    UriParse(hyper::http::uri::InvalidUri),
    UrlParse(url::ParseError),
    NoBackendsForPrefix(String),
    AllBackendsFailed { prefix: String, attempts: usize },
    InvalidPath,
    MissingHostHeader,
    RequestBodyTooLarge, // Or BodyReadError
    ClientIpParseError,
}

impl fmt::Display for ProxyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProxyError::Io(e) => write!(f, "IO error: {}", e),
            ProxyError::ConfigLoad(e) => write!(f, "Config load error: {}", e),
            ProxyError::Hyper(e) => write!(f, "Hyper error: {}", e),
            ProxyError::UriParse(e) => write!(f, "URI parse error: {}", e),
            ProxyError::UrlParse(e) => write!(f, "URL parse error: {}", e),
            ProxyError::NoBackendsForPrefix(p) => write!(f, "No backends configured for prefix: {}", p),
            ProxyError::AllBackendsFailed{ prefix, attempts } => {
                write!(f, "All {} backend(s) failed for prefix '{}'", attempts, prefix)
            }
            ProxyError::InvalidPath => write!(f, "Invalid request path"),
            ProxyError::MissingHostHeader => write!(f, "Missing Host header"),
            ProxyError::RequestBodyTooLarge => write!(f, "Request body too large or error reading body"),
            ProxyError::ClientIpParseError => write!(f, "Could not parse client IP"),
        }
    }
}

impl std::error::Error for ProxyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ProxyError::Io(e) => Some(e),
            ProxyError::ConfigLoad(e) => Some(e),
            ProxyError::Hyper(e) => Some(e),
            ProxyError::UriParse(e) => Some(e),
            ProxyError::UrlParse(e) => Some(e),
            _ => None,
        }
    }
}

// Converters
impl From<std::io::Error> for ProxyError {
    fn from(err: std::io::Error) -> Self {
        ProxyError::Io(err)
    }
}

impl From<serde_yaml::Error> for ProxyError {
    fn from(err: serde_yaml::Error) -> Self {
        ProxyError::ConfigLoad(err)
    }
}

impl From<hyper::Error> for ProxyError {
    fn from(err: hyper::Error) -> Self {
        ProxyError::Hyper(err)
    }
}

impl From<hyper::http::uri::InvalidUri> for ProxyError {
    fn from(err: hyper::http::uri::InvalidUri) -> Self {
        ProxyError::UriParse(err)
    }
}

impl From<url::ParseError> for ProxyError {
    fn from(err: url::ParseError) -> Self {
        ProxyError::UrlParse(err)
    }
}


// Helper to convert ProxyError to an HTTP Response
pub fn error_to_response(err: &ProxyError) -> Response<Full<Bytes>> {
    let status = match err {
        ProxyError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        ProxyError::ConfigLoad(_) => StatusCode::INTERNAL_SERVER_ERROR,
        ProxyError::Hyper(_) => StatusCode::BAD_GATEWAY, // Or INTERNAL_SERVER_ERROR if it's our client
        ProxyError::UriParse(_) => StatusCode::BAD_REQUEST,
        ProxyError::UrlParse(_) => StatusCode::INTERNAL_SERVER_ERROR, // config related
        ProxyError::NoBackendsForPrefix(_) => StatusCode::NOT_FOUND,
        ProxyError::AllBackendsFailed { .. } => StatusCode::SERVICE_UNAVAILABLE,
        ProxyError::InvalidPath => StatusCode::BAD_REQUEST,
        ProxyError::MissingHostHeader => StatusCode::BAD_REQUEST,
        ProxyError::RequestBodyTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        ProxyError::ClientIpParseError => StatusCode::INTERNAL_SERVER_ERROR,
    };
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::from(err.to_string())))
        .unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Full::new(Bytes::from("Error generating error response")))
                .unwrap()
        })
}


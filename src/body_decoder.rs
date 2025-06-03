use bytes::Bytes;
use hyper::HeaderMap;
use hyper::header::CONTENT_ENCODING;
use flate2::read::GzDecoder;
use std::io::Read;

/// Decodes the body bytes for logging purposes based on the Content-Encoding header.
pub fn decode_body_for_logging(body: &[u8], headers: &HeaderMap) -> Bytes {
    if let Some(content_encoding) = headers.get(CONTENT_ENCODING) {
        if content_encoding == "gzip" {
            let mut decoder = GzDecoder::new(&body[..]);
            let mut decompressed = Vec::new();
            if decoder.read_to_end(&mut decompressed).is_ok() {
                return Bytes::from(decompressed);
            }
        }
    }
    // Fallback to original body if no gzip encoding or on error
    Bytes::from(body.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use hyper::HeaderMap;
    use hyper::header::CONTENT_ENCODING;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    #[test]
    fn test_no_content_encoding() {
        let data = b"hello world";
        let headers = HeaderMap::new();
        let result = decode_body_for_logging(data, &headers);
        assert_eq!(result, Bytes::from_static(data));
    }

    #[test]
    fn test_non_gzip_encoding() {
        let data = b"hello world";
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_ENCODING, "deflate".parse().unwrap());
        let result = decode_body_for_logging(data, &headers);
        assert_eq!(result, Bytes::from_static(data));
    }

    #[test]
    fn test_gzip_decoding_success() {
        let original = b"the quick brown fox";
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(original).unwrap();
        let compressed = encoder.finish().unwrap();

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_ENCODING, "gzip".parse().unwrap());
        let result = decode_body_for_logging(&compressed, &headers);
        assert_eq!(result, Bytes::from_static(original));
    }

    #[test]
    fn test_gzip_decoding_failure() {
        // Provide invalid gzip data
        let data = b"not valid gzip";
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_ENCODING, "gzip".parse().unwrap());
        let result = decode_body_for_logging(data, &headers);
        assert_eq!(result, Bytes::from_static(data));
    }
}


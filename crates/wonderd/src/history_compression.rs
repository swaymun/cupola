//! Stateless response compression for authenticated, stored history only.
//! Never compress session/authentication responses, cookies, pairing proofs,
//! requests or general event frames. No dictionary spans responses.
use axum::{
    body::{to_bytes, Body},
    http::{header, HeaderMap, HeaderValue},
    response::Response,
};
use std::io::Write;

pub(crate) const MIN_BYTES: usize = 1024;
pub(crate) const MAX_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const RAW_DEFLATE: &str = "deflate-raw";

pub(crate) fn history_path(method: &str, path: &str) -> bool {
    let parts: Vec<_> = path.split('?').next().unwrap_or("").split('/').collect();
    method == "GET"
        && matches!(parts.as_slice(), ["", "api", "v1", "conversations", id] if !id.is_empty())
        || method == "GET"
            && matches!(parts.as_slice(), ["", "api", "v1", "conversations", id, "history"] if !id.is_empty())
}

fn accepts_gzip(headers: &HeaderMap) -> bool {
    let mut wildcard = false;
    for value in headers.get_all(header::ACCEPT_ENCODING) {
        let Ok(value) = value.to_str() else { continue };
        for part in value.split(',') {
            let mut parts = part.trim().split(';');
            let codec = parts.next().unwrap_or("").trim();
            let quality = parts
                .filter_map(|part| part.trim().split_once('='))
                .find(|(key, _)| key.eq_ignore_ascii_case("q"))
                .map_or(1.0, |(_, value)| value.parse::<f32>().unwrap_or(0.0));
            if codec.eq_ignore_ascii_case("gzip") {
                return quality > 0.0;
            }
            if codec == "*" {
                wildcard = quality > 0.0;
            }
        }
    }
    wildcard
}

pub(crate) async fn compress(bytes: Vec<u8>, raw: bool) -> (Vec<u8>, bool) {
    if bytes.len() < MIN_BYTES || bytes.len() > MAX_BYTES {
        return (bytes, false);
    }
    // A cancelled request keeps its permit with the actual CPU work.
    static WORK: std::sync::LazyLock<std::sync::Arc<tokio::sync::Semaphore>> =
        std::sync::LazyLock::new(|| std::sync::Arc::new(tokio::sync::Semaphore::new(2)));
    let permit = WORK
        .clone()
        .acquire_owned()
        .await
        .expect("compression semaphore is never closed");
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let encoded = if raw {
            let mut encoder =
                flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(6));
            encoder.write_all(&bytes).and_then(|_| encoder.finish())
        } else {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(6));
            encoder.write_all(&bytes).and_then(|_| encoder.finish())
        };
        match encoded {
            Ok(encoded) if encoded.len() + 128 < bytes.len() => (encoded, true),
            _ => (bytes, false),
        }
    })
    .await
    .expect("history compression worker panicked")
}

pub(crate) async fn response(request: &HeaderMap, mut response: Response) -> Response {
    use axum::body::HttpBody as _;
    response
        .headers_mut()
        .append(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    if !accepts_gzip(request)
        || response
            .body()
            .size_hint()
            .upper()
            .is_none_or(|bytes| bytes > MAX_BYTES as u64)
    {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = to_bytes(body, MAX_BYTES).await else {
        return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let (bytes, compressed) = compress(bytes.to_vec(), false).await;
    if compressed {
        parts
            .headers
            .insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        parts.headers.remove(header::CONTENT_LENGTH);
    }
    Response::from_parts(parts, Body::from(bytes))
}

use axum::response::IntoResponse;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[tokio::test]
    async fn history_codecs_round_trip_and_exclude_credentials_and_small_answers() {
        let bytes = "a stored history paragraph\n".repeat(1000).into_bytes();
        for raw in [false, true] {
            let (encoded, compressed) = compress(bytes.clone(), raw).await;
            assert!(compressed);
            let mut decoded = Vec::new();
            if raw {
                flate2::read::DeflateDecoder::new(encoded.as_slice())
                    .read_to_end(&mut decoded)
                    .unwrap();
            } else {
                flate2::read::GzDecoder::new(encoded.as_slice())
                    .read_to_end(&mut decoded)
                    .unwrap();
            }
            assert_eq!(decoded, bytes);
        }
        assert_eq!(
            compress(b"small".to_vec(), true).await,
            (b"small".to_vec(), false)
        );
        for path in [
            "/api/v1/session",
            "/api/v1/pairing",
            "/api/v1/conversations/c/history/refresh",
            "/api/v1/conversations/c/files/f",
        ] {
            assert!(!history_path("GET", path));
        }
        assert!(history_path(
            "GET",
            "/api/v1/conversations/c/history?limit=100"
        ));
        assert!(!history_path("POST", "/api/v1/conversations/c"));
        for (value, expected) in [
            ("br, gzip, deflate", true),
            ("gzip;q=0, *;q=1", false),
            ("br", false),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::ACCEPT_ENCODING, value.parse().unwrap());
            assert_eq!(accepts_gzip(&headers), expected);
        }
    }
}

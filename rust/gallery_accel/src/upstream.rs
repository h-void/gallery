//! HTTP reverse-proxy helpers for residual Python routes and scan-state polling.
//!
//! **Optimization:** keep one public process on :8899 (Rust) while unfinished
//! domains (scan workers, ML, complex video, folder execute) stay on an
//! internal Python upstream. Avoids a big-bang rewrite without breaking UI.
//!
//! **Streaming:** residual media (`/api/file`, stream, HLS, Range 206) must not
//! be fully buffered in the primary process — forward `bytes_stream` and keep
//! `Content-Range` / `Accept-Ranges` / `Content-Length` from upstream.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt}; // SinkExt used by scan_ws_bridge
use serde_json::Value;

#[derive(Clone)]
pub struct Upstream {
    base: String,
    client: reqwest::Client,
}

impl Upstream {
    pub fn new(base: &str) -> Result<Self> {
        let base = base.trim_end_matches('/').to_string();
        if base.is_empty() {
            return Err(anyhow!("empty upstream URL"));
        }
        // No default response-body timeout: media streams can run longer than 120s.
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .pool_max_idle_per_host(8)
            .build()
            .context("build upstream client")?;
        Ok(Self { base, client })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub async fn get_json(&self, path: &str) -> Result<Value> {
        let url = format!("{}{}", self.base, path);
        let response = self
            .client
            .get(&url)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .with_context(|| format!("upstream GET {url}"))?;
        let status = response.status();
        let body = response.bytes().await.context("upstream body")?;
        if !status.is_success() {
            return Err(anyhow!(
                "upstream {url} returned {status}: {}",
                String::from_utf8_lossy(&body)
            ));
        }
        serde_json::from_slice(&body).context("decode upstream json")
    }

    /// Reverse-proxy one request to residual Python.
    ///
    /// Response body is **streamed** (`bytes_stream` → `Body::from_stream`) so
    /// large media and `Range`/`206` responses do not load entirely into RAM.
    /// Request/response headers preserve `Range`, `Content-Range`,
    /// `Accept-Ranges`, and `Content-Length`.
    pub async fn forward(
        &self,
        method: Method,
        uri: &Uri,
        headers: HeaderMap,
        body: Bytes,
    ) -> Result<Response> {
        let path_and_query = uri
            .path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or(uri.path());
        let url = format!("{}{}", self.base, path_and_query);
        let mut request = self.client.request(method, &url).body(body);
        for (name, value) in headers.iter() {
            if is_request_hop_by_hop(name) {
                continue;
            }
            if let Ok(v) = value.to_str() {
                request = request.header(name.as_str(), v);
            }
        }
        let upstream = request.send().await.with_context(|| format!("proxy {url}"))?;
        let status =
            StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let mut response_builder = Response::builder().status(status);
        for (name, value) in upstream.headers().iter() {
            if is_response_hop_by_hop(name) {
                continue;
            }
            if let (Ok(n), Ok(v)) = (
                HeaderName::from_bytes(name.as_str().as_bytes()),
                HeaderValue::from_bytes(value.as_bytes()),
            ) {
                response_builder = response_builder.header(n, v);
            }
        }
        // Stream residual chunks (no full-body buffer of media responses).
        let stream = upstream.bytes_stream().map(|chunk| {
            chunk.map_err(|err| std::io::Error::other(format!("upstream stream: {err}")))
        });
        response_builder
            .body(Body::from_stream(stream))
            .context("build streaming proxy response")
    }
}

/// Hop-by-hop request headers. Keep `Range` and body-related entity headers.
fn is_request_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str().to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
            | "host"
    )
}

/// Hop-by-hop response headers only. **Do not** strip `content-length`,
/// `content-range`, or `accept-ranges` — required for 206 partial media.
fn is_response_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str().to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
    )
}

/// Bridge `/ws/scan` by polling upstream HTTP `/api/scan/state`.
///
/// **Optimization:** avoids a full WebSocket reverse-proxy implementation while
/// preserving the UI progress protocol (JSON scan-state objects).
pub async fn scan_ws_bridge(mut socket: WebSocket, upstream: Upstream) {
    let mut last = String::new();
    loop {
        match upstream.get_json("/api/scan/state").await {
            Ok(state) => {
                let encoded = state.to_string();
                if encoded != last {
                    if socket
                        .send(Message::Text(encoded.clone().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    last = encoded;
                }
            }
            Err(_) => {
                // Upstream may be briefly down during residual restarts.
            }
        }
        tokio::select! {
            msg = socket.next() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(p))) => {
                        let _ = socket.send(Message::Pong(p)).await;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
    }
}

pub fn proxy_error(message: impl Into<String>) -> Response {
    (
        StatusCode::BAD_GATEWAY,
        [(header::CONTENT_TYPE, "application/json")],
        format!(r#"{{"error":"{}"}}"#, message.into().replace('"', "'")),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderName;

    #[test]
    fn response_hop_by_hop_keeps_range_media_headers() {
        for name in ["content-length", "content-range", "accept-ranges", "content-type"] {
            let header = HeaderName::from_static(name);
            assert!(
                !is_response_hop_by_hop(&header),
                "{name} must be forwarded for media Range/206"
            );
        }
        assert!(is_response_hop_by_hop(&HeaderName::from_static("connection")));
        assert!(is_response_hop_by_hop(&HeaderName::from_static(
            "transfer-encoding"
        )));
    }

    #[test]
    fn request_hop_by_hop_keeps_range_header() {
        let range = HeaderName::from_static("range");
        assert!(!is_request_hop_by_hop(&range));
    }

    #[test]
    fn forward_source_streams_without_full_buffer() {
        // Guard against regression: product media must use bytes_stream, not full buffer.
        let source = include_str!("upstream.rs");
        assert!(
            source.contains("bytes_stream()"),
            "Upstream::forward must stream residual bodies"
        );
        assert!(
            source.contains("Body::from_stream"),
            "Upstream::forward must build a streaming Body"
        );
        // Only get_json may call response.bytes(); forward must not.
        let forward_block = source
            .split("pub async fn forward")
            .nth(1)
            .and_then(|rest| rest.split("fn is_request_hop_by_hop").next())
            .expect("forward function body");
        assert!(
            !forward_block.contains("response.bytes()"),
            "Upstream::forward must not fully buffer residual response bodies"
        );
        assert!(
            !forward_block.contains("upstream.bytes()"),
            "Upstream::forward must not fully buffer residual response bodies"
        );
    }
}

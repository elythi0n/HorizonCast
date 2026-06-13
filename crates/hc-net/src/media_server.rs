//! Local HTTP media server for DLNA casting.
//!
//! Serves a single media file to the renderer with HTTP `Range` support (so the TV can
//! seek) and the DLNA headers Samsung renderers expect. The pure helpers ([`parse_range`],
//! [`content_type_for`]) are unit-tested; [`MediaServer`] is covered by an integration
//! test that drives it over a real socket.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_util::io::ReaderStream;

use hc_core::{Error, Result};

/// DLNA `contentFeatures.dlna.org` value: range/seek supported (OP=01), streaming flags.
/// Shared so the DIDL `<res protocolInfo>` we send matches what the server advertises.
pub(crate) const DLNA_CONTENT_FEATURES: &str =
    "DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000";

struct ServerState {
    path: PathBuf,
    content_type: &'static str,
}

/// A running media server. Drop or call [`MediaServer::stop`] to shut it down; while it
/// lives, the file at the configured path is reachable at [`MediaServer::url`].
pub struct MediaServer {
    /// URL the renderer should play (built from the local IP it can reach us on).
    pub url: String,
    shutdown: Option<oneshot::Sender<()>>,
    handle: tokio::task::JoinHandle<()>,
}

impl MediaServer {
    /// Start serving `media` on an ephemeral port. `local_ip` is the address the
    /// renderer can reach us on (see [`crate::addr::local_ip_for`]) and is used to build
    /// [`MediaServer::url`]; the listener itself binds all interfaces.
    pub async fn start(media: &Path, local_ip: IpAddr) -> Result<Self> {
        if !tokio::fs::try_exists(media).await.unwrap_or(false) {
            return Err(Error::Sink(format!(
                "media file not found: {}",
                media.display()
            )));
        }
        let ext = media
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("bin")
            .to_ascii_lowercase();
        let state = Arc::new(ServerState {
            path: media.to_path_buf(),
            content_type: content_type_for(media),
        });

        let app = Router::new().fallback(serve).with_state(state);
        let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))
            .await
            .map_err(|e| Error::Sink(format!("media server bind failed: {e}")))?;
        let port = listener
            .local_addr()
            .map_err(|e| Error::Sink(format!("media server addr failed: {e}")))?
            .port();

        let (tx, rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            let shutdown = async {
                let _ = rx.await;
            };
            if let Err(e) = axum::serve(listener, app)
                .with_graceful_shutdown(shutdown)
                .await
            {
                tracing::warn!(error = %e, "media server exited with error");
            }
        });

        Ok(Self {
            url: format!("http://{local_ip}:{port}/media.{ext}"),
            shutdown: Some(tx),
            handle,
        })
    }

    /// Shut the server down and wait for it to finish.
    pub async fn stop(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = self.handle.await;
    }
}

/// Handle a GET/HEAD for the media file, honoring an optional `Range` request.
async fn serve(
    State(state): State<Arc<ServerState>>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    let range_header = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();

    let file = match tokio::fs::File::open(&state.path).await {
        Ok(f) => f,
        Err(e) => return (StatusCode::NOT_FOUND, e.to_string()).into_response(),
    };
    let total = match file.metadata().await {
        Ok(m) => m.len(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let is_head = method == Method::HEAD;
    let requested = range_header.as_deref().and_then(|v| parse_range(v, total));

    // A well-formed but unsatisfiable byte range gets 416 (RFC 7233).
    if requested.is_none()
        && let Some(r) = range_header.as_deref()
        && range_is_unsatisfiable(r, total)
    {
        tracing::info!(%method, range = r, ua = %user_agent, "media request → 416 unsatisfiable");
        let mut out = HeaderMap::new();
        out.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes */{total}")).expect("ascii content-range"),
        );
        return (StatusCode::RANGE_NOT_SATISFIABLE, out).into_response();
    }

    let mut out = HeaderMap::new();
    out.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    out.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(state.content_type),
    );
    out.insert(
        HeaderName::from_static("transfermode.dlna.org"),
        HeaderValue::from_static("Streaming"),
    );
    if headers.contains_key("getcontentfeatures.dlna.org") {
        out.insert(
            HeaderName::from_static("contentfeatures.dlna.org"),
            HeaderValue::from_static(DLNA_CONTENT_FEATURES),
        );
    }

    match requested {
        Some((start, end)) => {
            let len = end - start + 1;
            insert_len(&mut out, len);
            out.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes {start}-{end}/{total}"))
                    .expect("ascii content-range"),
            );
            tracing::info!(%method, range = %format!("{start}-{end}/{total}"), ua = %user_agent, "media request → 206");
            let body = if is_head {
                Body::empty()
            } else {
                ranged_body(file, start, len).await
            };
            (StatusCode::PARTIAL_CONTENT, out, body).into_response()
        }
        None => {
            insert_len(&mut out, total);
            tracing::info!(%method, bytes = total, ua = %user_agent, "media request → 200 full");
            let body = if is_head {
                Body::empty()
            } else {
                Body::from_stream(ReaderStream::new(file))
            };
            (StatusCode::OK, out, body).into_response()
        }
    }
}

/// True if `value` is a well-formed `bytes=` range that cannot be satisfied for `total`
/// (so it warrants a 416). Malformed/non-`bytes=` headers return `false` (we ignore them
/// and serve the whole file instead, per the lenient reading of RFC 7233).
fn range_is_unsatisfiable(value: &str, total: u64) -> bool {
    let Some(spec) = value.strip_prefix("bytes=") else {
        return false;
    };
    let Some((start, end)) = spec.split(',').next().unwrap_or("").trim().split_once('-') else {
        return false;
    };
    if start.is_empty() {
        matches!(end.parse::<u64>(), Ok(0)) // a zero-length suffix range
    } else {
        start.parse::<u64>().is_ok_and(|s| s >= total) // first byte past the end
    }
}

fn insert_len(out: &mut HeaderMap, len: u64) {
    out.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&len.to_string()).expect("numeric content-length"),
    );
}

async fn ranged_body(mut file: tokio::fs::File, start: u64, len: u64) -> Body {
    if let Err(e) = file.seek(SeekFrom::Start(start)).await {
        tracing::warn!(error = %e, "media server seek failed");
        return Body::empty();
    }
    Body::from_stream(ReaderStream::new(file.take(len)))
}

/// Map a file extension to a sensible `Content-Type` for TV playback.
#[must_use]
pub fn content_type_for(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("mp4" | "m4v") => "video/mp4",
        Some("mkv") => "video/x-matroska",
        Some("mov") => "video/quicktime",
        Some("webm") => "video/webm",
        Some("avi") => "video/x-msvideo",
        Some("ts") => "video/mp2t",
        Some("mpeg" | "mpg") => "video/mpeg",
        Some("mp3") => "audio/mpeg",
        Some("flac") => "audio/flac",
        Some("aac") => "audio/aac",
        Some("wav") => "audio/wav",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("png") => "image/png",
        _ => "application/octet-stream",
    }
}

/// Parse a single HTTP `Range` header value against a known `total` length, returning an
/// inclusive `(start, end)` byte range clamped to the file, or `None` if unsatisfiable.
#[must_use]
pub fn parse_range(value: &str, total: u64) -> Option<(u64, u64)> {
    if total == 0 {
        return None;
    }
    let last = total - 1;
    let spec = value.strip_prefix("bytes=")?;
    let spec = spec.split(',').next()?.trim(); // only the first range
    let (s, e) = spec.split_once('-')?;

    let (start, end) = if s.is_empty() {
        // Suffix form: bytes=-N => final N bytes.
        let n: u64 = e.parse().ok()?;
        if n == 0 {
            return None;
        }
        (total.saturating_sub(n), last)
    } else {
        let start: u64 = s.parse().ok()?;
        let end = if e.is_empty() {
            last
        } else {
            e.parse::<u64>().ok()?.min(last)
        };
        (start, end)
    };

    if start > end || start > last {
        return None;
    }
    Some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_type_by_extension() {
        assert_eq!(content_type_for(Path::new("a.mp4")), "video/mp4");
        assert_eq!(content_type_for(Path::new("A.MKV")), "video/x-matroska");
        assert_eq!(
            content_type_for(Path::new("clip.unknownext")),
            "application/octet-stream"
        );
        assert_eq!(
            content_type_for(Path::new("noext")),
            "application/octet-stream"
        );
    }

    #[test]
    fn parse_range_variants() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=100-", 1000), Some((100, 999)));
        assert_eq!(parse_range("bytes=-50", 1000), Some((950, 999)));
        assert_eq!(parse_range("bytes=500-100000", 1000), Some((500, 999))); // end clamped
        assert_eq!(parse_range("bytes=2000-3000", 1000), None); // start past end
        assert_eq!(parse_range("bytes=50-10", 1000), None); // start > end
        assert_eq!(parse_range("nonsense", 1000), None);
        assert_eq!(parse_range("bytes=0-0", 0), None); // empty file
        assert_eq!(parse_range("bytes=-0", 1000), None); // zero-length suffix
    }

    #[tokio::test]
    async fn serves_full_ranged_and_head() {
        let data: Vec<u8> = (0u32..5000).map(|i| (i % 256) as u8).collect();
        let path = std::env::temp_dir().join("hc_media_server_test.mp4");
        tokio::fs::write(&path, &data).await.unwrap();

        let server = MediaServer::start(&path, IpAddr::V4(Ipv4Addr::LOCALHOST))
            .await
            .unwrap();
        let url = server.url.clone();
        assert!(url.starts_with("http://127.0.0.1:"));
        assert!(url.ends_with("/media.mp4"));
        let client = reqwest::Client::new();

        // Full GET.
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers()[reqwest::header::ACCEPT_RANGES], "bytes");
        assert_eq!(resp.headers()[reqwest::header::CONTENT_TYPE], "video/mp4");
        assert_eq!(resp.headers()[reqwest::header::CONTENT_LENGTH], "5000");
        assert_eq!(resp.bytes().await.unwrap().as_ref(), data.as_slice());

        // Ranged GET.
        let resp = client
            .get(&url)
            .header(reqwest::header::RANGE, "bytes=100-199")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 206);
        assert_eq!(
            resp.headers()[reqwest::header::CONTENT_RANGE],
            "bytes 100-199/5000"
        );
        assert_eq!(resp.bytes().await.unwrap().as_ref(), &data[100..=199]);

        // HEAD: status + length, empty body.
        let resp = client.head(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers()[reqwest::header::CONTENT_LENGTH], "5000");
        assert!(resp.bytes().await.unwrap().is_empty());

        server.stop().await;
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn unsatisfiable_range_returns_416() {
        let path = std::env::temp_dir().join("hc_media_416_test.bin");
        tokio::fs::write(&path, vec![0u8; 100]).await.unwrap();
        let server = MediaServer::start(&path, IpAddr::V4(Ipv4Addr::LOCALHOST))
            .await
            .unwrap();
        let resp = reqwest::Client::new()
            .get(&server.url)
            .header(reqwest::header::RANGE, "bytes=500-600")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 416);
        assert_eq!(
            resp.headers()[reqwest::header::CONTENT_RANGE],
            "bytes */100"
        );
        server.stop().await;
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[test]
    fn unsatisfiable_range_detection() {
        assert!(range_is_unsatisfiable("bytes=500-600", 100)); // start past end
        assert!(range_is_unsatisfiable("bytes=-0", 100)); // zero-length suffix
        assert!(!range_is_unsatisfiable("bytes=0-50", 100)); // satisfiable
        assert!(!range_is_unsatisfiable("bytes=50-", 100)); // satisfiable open-ended
        assert!(!range_is_unsatisfiable("garbage", 100)); // not a bytes range → ignore
    }
}

//! Local HTTP server that serves a single open-ended **live** byte stream (e.g. live
//! MPEG-TS) to a DLNA renderer for screen mirroring.
//!
//! Unlike [`crate::media_server`] (a finite file with `Range` support), the live stream
//! has no `Content-Length`: the body is fed continuously from an in-process broadcast
//! channel via [`LiveStream::publish`], and each connected renderer receives chunks in
//! order. A renderer that falls behind drops data rather than stalling the stream.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, oneshot};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use hc_core::{Error, Result};

/// DLNA `contentFeatures` for a paced live stream: `OP=00` (no seek/range) + streaming flags.
/// Shared so the DIDL `<res protocolInfo>` for a live source matches what we serve.
pub(crate) const DLNA_LIVE_FEATURES: &str =
    "DLNA.ORG_OP=00;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=0D500000000000000000000000000000";

/// How many chunks may buffer per client before a slow client starts dropping.
const CHANNEL_CAPACITY: usize = 512;

struct LiveState {
    tx: broadcast::Sender<Bytes>,
    content_type: &'static str,
}

/// A running live-stream server. Publish encoded chunks with [`LiveStream::publish`];
/// connected renderers receive them in order. Stop with [`LiveStream::stop`].
pub struct LiveStream {
    /// URL the renderer should play.
    pub url: String,
    tx: broadcast::Sender<Bytes>,
    shutdown: Option<oneshot::Sender<()>>,
    handle: tokio::task::JoinHandle<()>,
}

impl LiveStream {
    /// Start the server on an ephemeral port. `local_ip` is the address the renderer can
    /// reach us on (used to build [`LiveStream::url`]); `content_type` is e.g.
    /// `video/mp2t` and `ext` the URL extension (e.g. `ts`).
    pub async fn start(local_ip: IpAddr, content_type: &'static str, ext: &str) -> Result<Self> {
        let (tx, _rx) = broadcast::channel(CHANNEL_CAPACITY);
        let state = Arc::new(LiveState {
            tx: tx.clone(),
            content_type,
        });
        let app = Router::new().fallback(serve).with_state(state);
        let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))
            .await
            .map_err(|e| Error::Sink(format!("live server bind failed: {e}")))?;
        let port = listener
            .local_addr()
            .map_err(|e| Error::Sink(format!("live server addr failed: {e}")))?
            .port();

        let (sh_tx, sh_rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            let shutdown = async {
                let _ = sh_rx.await;
            };
            if let Err(e) = axum::serve(listener, app)
                .with_graceful_shutdown(shutdown)
                .await
            {
                tracing::warn!(error = %e, "live stream server exited with error");
            }
        });

        Ok(Self {
            url: format!("http://{local_ip}:{port}/live.{ext}"),
            tx,
            shutdown: Some(sh_tx),
            handle,
        })
    }

    /// Publish a chunk to all connected renderers (a no-op if none are connected yet).
    /// Accepts any `Into<Bytes>` (e.g. `Vec<u8>`) so callers needn't depend on `bytes`.
    pub fn publish(&self, chunk: impl Into<Bytes>) {
        let _ = self.tx.send(chunk.into());
    }

    /// Number of renderers currently connected.
    #[must_use]
    pub fn client_count(&self) -> usize {
        self.tx.receiver_count()
    }

    /// Shut the server down and wait for it to finish.
    pub async fn stop(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = self.handle.await;
    }
}

async fn serve(State(state): State<Arc<LiveState>>, headers: HeaderMap) -> Response {
    let rx = state.tx.subscribe();
    // A lagging client drops data (None) instead of stalling or erroring the stream.
    let stream = BroadcastStream::new(rx).filter_map(|r| match r {
        Ok(bytes) => Some(Ok::<Bytes, std::io::Error>(bytes)),
        Err(_lagged) => None,
    });

    let mut out = HeaderMap::new();
    out.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(state.content_type),
    );
    out.insert(header::ACCEPT_RANGES, HeaderValue::from_static("none"));
    out.insert(
        HeaderName::from_static("transfermode.dlna.org"),
        HeaderValue::from_static("Streaming"),
    );
    if headers.contains_key("getcontentfeatures.dlna.org") {
        out.insert(
            HeaderName::from_static("contentfeatures.dlna.org"),
            HeaderValue::from_static(DLNA_LIVE_FEATURES),
        );
    }

    (StatusCode::OK, out, Body::from_stream(stream)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{Duration, sleep};

    #[tokio::test]
    async fn delivers_published_chunks_to_a_connected_client() {
        let live = LiveStream::start(IpAddr::V4(Ipv4Addr::LOCALHOST), "video/mp2t", "ts")
            .await
            .unwrap();
        assert!(live.url.ends_with("/live.ts"));
        let url = live.url.clone();

        // Client connects and reads the open-ended body until it sees the pattern.
        let client = tokio::spawn(async move {
            let resp = reqwest::get(&url).await.unwrap();
            assert_eq!(resp.status(), 200);
            assert_eq!(resp.headers()[reqwest::header::CONTENT_TYPE], "video/mp2t");
            let mut s = resp.bytes_stream();
            let mut got = Vec::new();
            while let Some(chunk) = s.next().await {
                got.extend_from_slice(&chunk.unwrap());
                if got.len() >= 9 {
                    break;
                }
            }
            got
        });

        // Publish repeatedly so the test doesn't depend on exact connect timing (a
        // broadcast only reaches subscribers present at send time).
        for _ in 0..200u32 {
            live.publish(Bytes::from_static(b"abc"));
            sleep(Duration::from_millis(5)).await;
        }

        let got = client.await.unwrap();
        assert!(
            got.windows(3).any(|w| w == b"abc"),
            "stream must carry published bytes"
        );
        live.stop().await;
    }

    #[tokio::test]
    async fn publish_without_clients_is_a_noop() {
        let live = LiveStream::start(IpAddr::V4(Ipv4Addr::LOCALHOST), "video/mp2t", "ts")
            .await
            .unwrap();
        assert_eq!(live.client_count(), 0);
        live.publish(Bytes::from_static(b"ignored")); // must not panic
        live.stop().await;
    }
}

//! DLNA media-cast flow: serve a local file over HTTP and drive the renderer's
//! AVTransport service (`SetAVTransportURI` + `Play`), with a handle to stop playback.

use std::net::IpAddr;
use std::path::Path;

use hc_core::Result;
use hc_net::live_stream::LiveStream;
use hc_net::media_server::{self, MediaServer};
use hc_net::mpegts::MpegTsMuxer;
use hc_net::{addr, upnp};

/// An active media-cast session. While held, playback continues; for a local file it
/// also keeps the local media server alive (the TV streams from it). Dropping or
/// [`stop`](MediaCastSession::stop)ping it tears everything down.
pub struct MediaCastSession {
    control_url: String,
    media_url: String,
    /// `Some` when we host the media ourselves (local file); `None` for an external URL.
    server: Option<MediaServer>,
}

impl MediaCastSession {
    /// The URL the renderer is playing from.
    #[must_use]
    pub fn media_url(&self) -> &str {
        &self.media_url
    }

    /// Tell the renderer to `Stop`, then shut down the local media server if any.
    pub async fn stop(self) -> Result<()> {
        let result = upnp::send_soap_action(&self.control_url, "Stop", upnp::build_stop()).await;
        if let Some(server) = self.server {
            server.stop().await;
        }
        result.map(|_| ())
    }
}

/// Cast a local media file to a DLNA renderer.
///
/// Starts a local HTTP server for `media`, then issues `SetAVTransportURI` + `Play` to
/// the renderer at `control_url`. `device_addr` is the renderer's IP, used to choose the
/// local interface it can reach us on; `title` is shown in the renderer's metadata. On
/// any failure the media server is torn down before returning.
pub async fn cast_file(
    control_url: &str,
    device_addr: IpAddr,
    media: &Path,
    title: &str,
) -> Result<MediaCastSession> {
    let local_ip = addr::local_ip_for(device_addr).await?;
    let server = MediaServer::start(media, local_ip).await?;
    let media_url = server.url.clone();
    // The DIDL protocolInfo MIME must match how the media server serves the file.
    let mime = media_server::content_type_for(media);

    if let Err(e) = start_playback(control_url, &media_url, title, mime).await {
        server.stop().await;
        return Err(e);
    }

    tracing::info!(%control_url, url = %media_url, "DLNA cast (file) started");
    Ok(MediaCastSession {
        control_url: control_url.to_string(),
        media_url,
        server: Some(server),
    })
}

/// Cast a media URL the renderer can already reach (e.g. a web video) directly — no
/// local media server is started. `title` is shown in the renderer's metadata.
pub async fn cast_url(control_url: &str, url: &str, title: &str) -> Result<MediaCastSession> {
    let mime = mime_for_url(url);
    start_playback(control_url, url, title, mime).await?;

    tracing::info!(%control_url, %url, "DLNA cast (url) started");
    Ok(MediaCastSession {
        control_url: control_url.to_string(),
        media_url: url.to_string(),
        server: None,
    })
}

/// Issue `SetAVTransportURI` then `Play` for `media_url` on the renderer.
async fn start_playback(control_url: &str, media_url: &str, title: &str, mime: &str) -> Result<()> {
    let didl = upnp::didl_lite_video(title, media_url, mime);
    let set_body = upnp::build_set_av_transport_uri(media_url, &didl);
    upnp::send_soap_action(control_url, "SetAVTransportURI", set_body).await?;
    upnp::send_soap_action(control_url, "Play", upnp::build_play()).await?;
    Ok(())
}

/// Best-effort MIME from a URL's path extension (query/fragment stripped).
fn mime_for_url(url: &str) -> &'static str {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    media_server::content_type_for(Path::new(path))
}

/// Recover a renderer's absolute AVTransport control URL from its description-document
/// `location`. Useful when discovery's enrichment failed (e.g. the description fetch
/// timed out) but the SSDP `LOCATION` is still known. Returns `Ok(None)` if the device
/// has no AVTransport service.
pub async fn resolve_control_url(location: &str) -> Result<Option<String>> {
    let xml = upnp::fetch_description(location).await?;
    Ok(upnp::parse_device_description(&xml)
        .av_transport_control_url
        .and_then(|control| upnp::resolve_url(location, &control)))
}

/// An active live screen-mirror session: serves a live MPEG-TS stream the renderer plays,
/// fed by [`push_access_unit`](LiveMirrorSession::push_access_unit). A capture→encode loop
/// supplies H.264 access units; this session muxes them to TS and streams them.
pub struct LiveMirrorSession {
    control_url: String,
    live: LiveStream,
    muxer: MpegTsMuxer,
}

impl LiveMirrorSession {
    /// Start a live mirror: spin up the live MPEG-TS stream server, then point the
    /// renderer at it (`SetAVTransportURI` + `Play`). Feed frames with
    /// [`push_access_unit`](Self::push_access_unit).
    pub async fn start(control_url: &str, device_addr: IpAddr, title: &str) -> Result<Self> {
        let local_ip = addr::local_ip_for(device_addr).await?;
        let live = LiveStream::start(local_ip, "video/mp2t", "ts").await?;
        let didl = upnp::didl_lite_live_video(title, &live.url, "video/mp2t");
        let set_body = upnp::build_set_av_transport_uri(&live.url, &didl);

        if let Err(e) = upnp::send_soap_action(control_url, "SetAVTransportURI", set_body).await {
            live.stop().await;
            return Err(e);
        }
        if let Err(e) = upnp::send_soap_action(control_url, "Play", upnp::build_play()).await {
            live.stop().await;
            return Err(e);
        }

        tracing::info!(%control_url, url = %live.url, "DLNA live mirror started");
        Ok(Self {
            control_url: control_url.to_string(),
            live,
            muxer: MpegTsMuxer::new(),
        })
    }

    /// The live-stream URL the renderer is playing.
    #[must_use]
    pub fn stream_url(&self) -> &str {
        &self.live.url
    }

    /// Mux one H.264 access unit (Annex-B, 90 kHz `pts`) into MPEG-TS and stream it to the
    /// renderer. `keyframe` emits PAT/PMT so a renderer can join at any keyframe.
    pub fn push_access_unit(&mut self, annexb: &[u8], pts: u64, keyframe: bool) {
        let ts = self.muxer.push_access_unit(annexb, pts, keyframe);
        self.live.publish(ts);
    }

    /// Mux one AAC (ADTS) audio access unit (90 kHz `pts`) and stream it to the renderer.
    pub fn push_audio_access_unit(&mut self, adts: &[u8], pts: u64) {
        let ts = self.muxer.push_audio_access_unit(adts, pts);
        self.live.publish(ts);
    }

    /// Stop playback on the renderer and shut the live stream down.
    pub async fn stop(self) -> Result<()> {
        let result = upnp::send_soap_action(&self.control_url, "Stop", upnp::build_stop()).await;
        self.live.stop().await;
        result.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::routing::post;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    /// Records what a (mock) DLNA renderer's AVTransport control endpoint received.
    #[derive(Default)]
    struct Recorder {
        actions: Vec<String>,
        current_uri: Option<String>,
    }

    fn between<'a>(s: &'a str, start: &str, end: &str) -> Option<&'a str> {
        let i = s.find(start)? + start.len();
        let j = s[i..].find(end)? + i;
        Some(&s[i..j])
    }

    async fn control(
        State(rec): State<Arc<Mutex<Recorder>>>,
        headers: HeaderMap,
        body: String,
    ) -> String {
        let action = headers
            .get("soapaction")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        {
            let mut r = rec.lock().expect("recorder mutex");
            if action.contains("SetAVTransportURI")
                && let Some(uri) = between(&body, "<CurrentURI>", "</CurrentURI>")
            {
                r.current_uri = Some(uri.to_string());
            }
            r.actions.push(action);
        }
        // Minimal well-formed SOAP 200 response (cast_file only checks the status).
        "<?xml version=\"1.0\"?><s:Envelope \
         xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body/></s:Envelope>"
            .to_string()
    }

    /// Start a mock renderer control endpoint; returns its control URL and a shutdown tx.
    async fn start_mock_renderer(rec: Arc<Mutex<Recorder>>) -> (String, oneshot::Sender<()>) {
        let app = Router::new()
            .route("/control", post(control))
            .with_state(rec);
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });
        (format!("http://127.0.0.1:{port}/control"), tx)
    }

    #[tokio::test]
    async fn cast_file_drives_renderer_and_serves_media() {
        // End-to-end (sans physical TV): cast_file must (1) tell the renderer to load our
        // media URL, (2) Play, (3) actually serve those exact bytes at that URL, and
        // (4) Stop on teardown.
        let data: Vec<u8> = (0u32..3000).map(|i| (i % 256) as u8).collect();
        let path = std::env::temp_dir().join("hc_cast_integration.mp4");
        tokio::fs::write(&path, &data).await.unwrap();

        let rec = Arc::new(Mutex::new(Recorder::default()));
        let (control_url, shutdown) = start_mock_renderer(rec.clone()).await;

        let session = cast_file(&control_url, IpAddr::V4(Ipv4Addr::LOCALHOST), &path, "Clip")
            .await
            .expect("cast_file should succeed against the mock renderer");

        let media_url = session.media_url().to_string();
        {
            let r = rec.lock().unwrap();
            assert!(
                r.actions.iter().any(|a| a.contains("SetAVTransportURI")),
                "renderer must receive SetAVTransportURI"
            );
            assert!(
                r.actions.iter().any(|a| a.contains("Play")),
                "renderer must receive Play"
            );
            assert_eq!(
                r.current_uri.as_deref(),
                Some(media_url.as_str()),
                "renderer must be pointed at our media server URL"
            );
        }

        // The advertised URL must actually serve the file's bytes (what the TV fetches).
        let fetched = reqwest::get(&media_url)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(
            fetched.as_ref(),
            data.as_slice(),
            "served bytes must match the file"
        );

        session.stop().await.unwrap();
        assert!(
            rec.lock()
                .unwrap()
                .actions
                .iter()
                .any(|a| a.contains("Stop")),
            "renderer must receive Stop on teardown"
        );

        let _ = shutdown.send(());
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn cast_url_points_renderer_at_external_url_without_a_server() {
        // No media server is started; the renderer is simply pointed at the given URL.
        let rec = Arc::new(Mutex::new(Recorder::default()));
        let (control_url, shutdown) = start_mock_renderer(rec.clone()).await;

        let ext = "http://example.test/path/video.mp4?token=abc";
        let session = cast_url(&control_url, ext, "Clip").await.unwrap();
        assert_eq!(session.media_url(), ext);
        {
            let r = rec.lock().unwrap();
            assert_eq!(r.current_uri.as_deref(), Some(ext));
            assert!(r.actions.iter().any(|a| a.contains("SetAVTransportURI")));
            assert!(r.actions.iter().any(|a| a.contains("Play")));
        }

        session.stop().await.unwrap();
        assert!(
            rec.lock()
                .unwrap()
                .actions
                .iter()
                .any(|a| a.contains("Stop"))
        );
        let _ = shutdown.send(());
    }

    #[test]
    fn mime_for_url_uses_extension_ignoring_query() {
        assert_eq!(mime_for_url("http://h/v.mp4?token=1"), "video/mp4");
        assert_eq!(mime_for_url("http://h/a/b/clip.mkv"), "video/x-matroska");
        assert_eq!(mime_for_url("http://h/stream"), "application/octet-stream");
    }

    #[tokio::test]
    async fn resolve_control_url_from_description() {
        // Serve a Samsung-like description doc and confirm we recover an absolute
        // AVTransport control URL resolved against the description location.
        const DESC: &str = "<?xml version=\"1.0\"?>\
            <root xmlns=\"urn:schemas-upnp-org:device-1-0\"><device>\
            <friendlyName>TV</friendlyName>\
            <serviceList><service>\
            <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>\
            <controlURL>/upnp/control/AVTransport1</controlURL>\
            </service></serviceList></device></root>";

        async fn desc() -> ([(axum::http::HeaderName, &'static str); 1], &'static str) {
            ([(axum::http::header::CONTENT_TYPE, "text/xml")], DESC)
        }
        let app = Router::new().route("/desc.xml", axum::routing::get(desc));
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });

        let location = format!("http://127.0.0.1:{port}/desc.xml");
        let resolved = resolve_control_url(&location).await.unwrap();
        assert_eq!(
            resolved.as_deref(),
            Some(format!("http://127.0.0.1:{port}/upnp/control/AVTransport1").as_str())
        );
        let _ = tx.send(());
    }

    #[tokio::test]
    async fn live_mirror_issues_control_and_accepts_access_units() {
        // The session must point the renderer at its live-stream URL (SetAVTransportURI +
        // Play), then accept muxed access units without error, then Stop on teardown.
        let rec = Arc::new(Mutex::new(Recorder::default()));
        let (control_url, shutdown) = start_mock_renderer(rec.clone()).await;

        let mut session =
            LiveMirrorSession::start(&control_url, IpAddr::V4(Ipv4Addr::LOCALHOST), "Screen")
                .await
                .unwrap();
        let url = session.stream_url().to_string();
        assert!(url.ends_with("/live.ts"));
        {
            let r = rec.lock().unwrap();
            assert!(r.actions.iter().any(|a| a.contains("SetAVTransportURI")));
            assert!(r.actions.iter().any(|a| a.contains("Play")));
            assert_eq!(r.current_uri.as_deref(), Some(url.as_str()));
        }

        // Feeding access units muxes + publishes without error (no player connected yet).
        session.push_access_unit(&[0, 0, 0, 1, 0x65, 1, 2, 3], 0, true);
        session.push_access_unit(&[0, 0, 0, 1, 0x41, 4, 5, 6], 3000, false);

        session.stop().await.unwrap();
        assert!(
            rec.lock()
                .unwrap()
                .actions
                .iter()
                .any(|a| a.contains("Stop"))
        );
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn cast_file_errors_when_media_missing() {
        // No SOAP is attempted because the media server refuses to start; bounded and
        // offline-safe (loopback target, nonexistent file).
        let missing = std::env::temp_dir().join("hc_definitely_missing_media.mp4");
        let _ = std::fs::remove_file(&missing);
        let res = cast_file(
            "http://127.0.0.1:1/ctrl",
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            &missing,
            "Test",
        )
        .await;
        assert!(res.is_err(), "missing media file must fail fast");
    }
}

//! `hc-app` — HorizonCast desktop GUI (Slint).
//!
//! The Slint UI runs on the main thread; a background tokio runtime runs continuous
//! discovery and handles cast commands. Updates cross to the UI via
//! `slint::invoke_from_event_loop`; UI callbacks send commands over a channel.
//!
//! Three ways to cast (chosen from the in-app sheet):
//!   • **Mirror** — live screen capture → H.264 → MPEG-TS live stream (`LiveMirrorSession`).
//!   • **File**   — serve a local video over HTTP and play it (`cast_file`).
//!   • **Link**   — point the renderer at an external URL (`cast_url`).
//!
//! NOTE: like every native GUI, this only *builds* on a platform with the windowing
//! system libraries present (Windows/macOS out of the box; Linux needs
//! fontconfig/X libs). It is built and validated on the owner's machine.
#![windows_subsystem = "windows"]

slint::include_modules!();

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use hc_core::{Device, Protocol};
use hc_sink::dlna::{LiveMirrorSession, MediaCastSession};
use slint::{ModelRc, SharedString, VecModel};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// Commands the UI sends to the background runtime.
enum Cmd {
    CastFile {
        id: String,
        path: PathBuf,
    },
    CastUrl {
        id: String,
        url: String,
    },
    Mirror {
        id: String,
        fps: u32,
        bitrate_kbps: u32,
    },
    Stop,
}

/// Whatever is currently casting (so we can tear it down cleanly before starting anew).
enum Active {
    None,
    /// A file/URL media-cast.
    Media(MediaCastSession),
    /// A live screen mirror: the frame pump owns the session; signal `stop` to end it.
    Mirror {
        stop: oneshot::Sender<()>,
        pump: JoinHandle<()>,
        capture: JoinHandle<()>,
    },
}

fn protocol_label(device: &Device) -> &'static str {
    match device.protocols.first() {
        Some(Protocol::Dlna) => "DLNA",
        Some(Protocol::AirPlayMirror | Protocol::AirPlayVideo) => "AirPlay",
        Some(Protocol::Miracast) => "Miracast",
        Some(Protocol::Cast) => "Cast",
        None => "Unknown",
    }
}

fn to_item(device: &Device) -> DeviceItem {
    DeviceItem {
        id: device.id.clone().into(),
        name: device.name.clone().into(),
        protocol: protocol_label(device).into(),
    }
}

/// Resolve a device's DLNA control URL (recovering from its description if needed).
async fn control_url(device: &Device) -> Option<String> {
    if let Some(url) = device.dlna_control_url.clone() {
        return Some(url);
    }
    let location = device.dlna_location.clone()?;
    hc_sink::dlna::resolve_control_url(&location)
        .await
        .ok()
        .flatten()
}

/// Look up a snapshotted device by id.
fn find_device(snapshot: &Arc<Mutex<Vec<Device>>>, id: &str) -> Option<Device> {
    snapshot
        .lock()
        .expect("snapshot")
        .iter()
        .find(|d| d.id == id)
        .cloned()
}

/// Reset the UI to the not-casting state (called on any cast failure / mirror end).
fn clear_casting(weak: &slint::Weak<AppWindow>) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(app) = weak.upgrade() {
            app.set_casting(false);
            app.set_now_casting(SharedString::new());
        }
    });
}

/// Tear down whatever is currently active, awaiting a clean stop.
async fn teardown(active: Active) {
    match active {
        Active::None => {}
        Active::Media(session) => {
            let _ = session.stop().await;
        }
        Active::Mirror {
            stop,
            pump,
            capture,
        } => {
            let _ = stop.send(());
            let _ = pump.await;
            let _ = capture.await;
        }
    }
}

/// Blocking capture→encode loop: pulls NV12 frames, encodes H.264, and sends Annex-B
/// access units to `tx`. Stops when capture ends or the receiver is dropped. Mirrors the
/// headless `hc-cli mirror` pipeline.
fn capture_encode_loop(fps: u32, bitrate_kbps: u32, tx: &mpsc::Sender<hc_encode::EncodedFrame>) {
    let mut capture = match hc_capture::ScreenCapture::start(fps) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "screen capture unavailable");
            return;
        }
    };
    let Some(first) = capture.next_frame() else {
        capture.stop();
        return;
    };
    let config = hc_encode::EncoderConfig {
        width: first.width,
        height: first.height,
        fps,
        bitrate_kbps,
        keyframe_interval: fps.saturating_mul(2).max(1),
    };
    let mut encoder = match hc_encode::H264Encoder::new(config) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(error = %e, "encoder init failed");
            capture.stop();
            return;
        }
    };
    let mut clock = hc_core::PtsClock::new();

    let mut frame = Some(first);
    while let Some(f) = frame {
        let pts = clock.pts(f.pts);
        match encoder.encode(&f, pts) {
            Ok(units) => {
                for u in units {
                    if tx.blocking_send(u).is_err() {
                        capture.stop();
                        return;
                    }
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "encode error");
                break;
            }
        }
        frame = capture.next_frame();
    }
    let _ = encoder.finish();
    capture.stop();
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let app = AppWindow::new()?;

    // Snapshot of the latest discovered devices, shared with the runtime for cast lookup.
    let snapshot: Arc<Mutex<Vec<Device>>> = Arc::new(Mutex::new(Vec::new()));
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<Cmd>();

    // Background: tokio runtime running discovery + cast control.
    let weak = app.as_weak();
    let snap_bg = snapshot.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(async move {
            let discovery = hc_discovery::Discovery::start();
            let mut updates = discovery.subscribe();
            let mut active = Active::None;

            loop {
                tokio::select! {
                    changed = updates.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        let devices = updates.borrow_and_update().clone();
                        *snap_bg.lock().expect("snapshot") = devices.clone();
                        let items: Vec<DeviceItem> = devices.iter().map(to_item).collect();
                        let weak2 = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(app) = weak2.upgrade() {
                                app.set_devices(ModelRc::new(VecModel::from(items)));
                                app.set_scanning(false);
                            }
                        });
                    }
                    cmd = cmd_rx.recv() => match cmd {
                        Some(Cmd::CastFile { id, path }) => {
                            teardown(std::mem::replace(&mut active, Active::None)).await;
                            active = start_media(&snap_bg, &weak, &id, MediaSource::File(path)).await;
                        }
                        Some(Cmd::CastUrl { id, url }) => {
                            teardown(std::mem::replace(&mut active, Active::None)).await;
                            active = start_media(&snap_bg, &weak, &id, MediaSource::Url(url)).await;
                        }
                        Some(Cmd::Mirror { id, fps, bitrate_kbps }) => {
                            teardown(std::mem::replace(&mut active, Active::None)).await;
                            active = start_mirror(&snap_bg, &weak, &id, fps, bitrate_kbps).await;
                        }
                        Some(Cmd::Stop) => {
                            teardown(std::mem::replace(&mut active, Active::None)).await;
                        }
                        None => break,
                    },
                }
            }

            teardown(active).await;
            discovery.stop().await;
        });
    });

    // --- UI callbacks (main thread) ---

    app.on_select({
        let weak = app.as_weak();
        move |index| {
            if let Some(app) = weak.upgrade() {
                app.set_selected(index);
            }
        }
    });

    // Mirror the screen live.
    app.on_mirror({
        let weak = app.as_weak();
        let snap = snapshot.clone();
        let tx = cmd_tx.clone();
        move || {
            let Some(app) = weak.upgrade() else { return };
            let Some(device) = selected_device(&app, &snap) else {
                return;
            };
            let fps = if app.get_fps() == 1 { 30 } else { 60 };
            // Capture is at the display's native resolution; this picks the H.264 bitrate
            // ceiling for that quality tier (Auto/1080p/1440p/4K).
            let bitrate_kbps = match app.get_quality() {
                3 => 45_000, // 4K
                2 => 24_000, // 1440p (2K)
                1 => 12_000, // 1080p
                _ => 16_000, // Auto
            };
            app.set_casting(true);
            app.set_now_casting(format!("{} · screen", device.name).into());
            let _ = tx.send(Cmd::Mirror {
                id: device.id,
                fps,
                bitrate_kbps,
            });
        }
    });

    // Play a local video file.
    app.on_cast_file({
        let weak = app.as_weak();
        let snap = snapshot.clone();
        let tx = cmd_tx.clone();
        move || {
            let Some(app) = weak.upgrade() else { return };
            let Some(device) = selected_device(&app, &snap) else {
                return;
            };
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("Video", &["mp4", "m4v", "mov", "mkv", "webm"])
                .pick_file()
            {
                app.set_casting(true);
                app.set_now_casting(device.name.clone().into());
                let _ = tx.send(Cmd::CastFile {
                    id: device.id,
                    path,
                });
            }
        }
    });

    // Cast an external link/URL.
    app.on_cast_url({
        let weak = app.as_weak();
        let snap = snapshot.clone();
        let tx = cmd_tx.clone();
        move |url| {
            let Some(app) = weak.upgrade() else { return };
            let url = url.trim().to_string();
            if url.is_empty() {
                return;
            }
            let Some(device) = selected_device(&app, &snap) else {
                return;
            };
            app.set_casting(true);
            app.set_now_casting(device.name.clone().into());
            app.set_url_text(SharedString::new());
            let _ = tx.send(Cmd::CastUrl { id: device.id, url });
        }
    });

    app.on_stop({
        let weak = app.as_weak();
        let tx = cmd_tx.clone();
        move || {
            if let Some(app) = weak.upgrade() {
                app.set_casting(false);
                app.set_now_casting(SharedString::new());
            }
            let _ = tx.send(Cmd::Stop);
        }
    });

    app.on_rescan({
        let weak = app.as_weak();
        move || {
            if let Some(app) = weak.upgrade() {
                app.set_scanning(true);
            }
        }
    });

    app.on_check_updates(|| {
        tracing::info!("check for updates requested");
    });

    app.on_open_repo(|| {
        if let Err(e) = open::that("https://github.com/elythi0n/HorizonCast") {
            tracing::warn!(error = %e, "could not open repository URL");
        }
    });

    app.run()?;
    Ok(())
}

/// What a media-cast plays.
enum MediaSource {
    File(PathBuf),
    Url(String),
}

/// Resolve the currently-selected device from the UI index + the shared snapshot.
fn selected_device(app: &AppWindow, snap: &Arc<Mutex<Vec<Device>>>) -> Option<Device> {
    let index = app.get_selected();
    if index < 0 {
        return None;
    }
    snap.lock().expect("snapshot").get(index as usize).cloned()
}

/// Start a file/URL media-cast; returns the resulting `Active`, resetting the UI on failure.
async fn start_media(
    snapshot: &Arc<Mutex<Vec<Device>>>,
    weak: &slint::Weak<AppWindow>,
    id: &str,
    source: MediaSource,
) -> Active {
    let Some(device) = find_device(snapshot, id) else {
        clear_casting(weak);
        return Active::None;
    };
    let Some(control) = control_url(&device).await else {
        tracing::error!(device = %device.name, "no DLNA media endpoint");
        clear_casting(weak);
        return Active::None;
    };

    let result = match source {
        MediaSource::File(path) => {
            let title = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("HorizonCast")
                .to_string();
            hc_sink::dlna::cast_file(&control, device.address, &path, &title).await
        }
        MediaSource::Url(url) => hc_sink::dlna::cast_url(&control, &url, &device.name).await,
    };

    match result {
        Ok(session) => Active::Media(session),
        Err(e) => {
            tracing::error!(error = %e, "cast failed");
            clear_casting(weak);
            Active::None
        }
    }
}

/// Start a live screen mirror: capture+encode on a blocking task, fed into a
/// `LiveMirrorSession` by an async pump that ends on the `stop` signal (or capture death).
async fn start_mirror(
    snapshot: &Arc<Mutex<Vec<Device>>>,
    weak: &slint::Weak<AppWindow>,
    id: &str,
    fps: u32,
    bitrate_kbps: u32,
) -> Active {
    let Some(device) = find_device(snapshot, id) else {
        clear_casting(weak);
        return Active::None;
    };
    let Some(control) = control_url(&device).await else {
        tracing::error!(device = %device.name, "no DLNA media endpoint to mirror to");
        clear_casting(weak);
        return Active::None;
    };

    let session = match LiveMirrorSession::start(&control, device.address, "HorizonCast").await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "could not start live mirror");
            clear_casting(weak);
            return Active::None;
        }
    };

    let (tx, mut rx) = mpsc::channel::<hc_encode::EncodedFrame>(120);
    let capture = tokio::task::spawn_blocking(move || capture_encode_loop(fps, bitrate_kbps, &tx));

    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
    let weak_pump = weak.clone();
    let pump = tokio::spawn(async move {
        let mut session = session;
        loop {
            tokio::select! {
                _ = &mut stop_rx => break,
                unit = rx.recv() => match unit {
                    Some(u) => session.push_access_unit(&u.data, u.pts, u.keyframe),
                    None => {
                        // Capture/encode ended on its own (e.g. unsupported platform).
                        tracing::warn!("mirror capture ended");
                        clear_casting(&weak_pump);
                        break;
                    }
                },
            }
        }
        let _ = session.stop().await;
    });

    Active::Mirror {
        stop: stop_tx,
        pump,
        capture,
    }
}

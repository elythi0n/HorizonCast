//! `hc-cli` — headless harness to exercise discovery and (later) the capture → encode
//! → sink pipeline without the GUI, and to measure latency.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
use hc_core::{Device, Protocol};

/// HorizonCast headless harness.
#[derive(Parser)]
#[command(name = "hc-cli", version, about)]
struct Cli {
    /// Print live pipeline stats while running.
    #[arg(long, global = true)]
    stats: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scan the local network and list discovered cast devices.
    Devices {
        /// How many seconds to scan for.
        #[arg(long, default_value_t = 4)]
        secs: u64,
    },
    /// Continuously watch for devices, printing the list whenever it changes.
    Watch,
    /// Probe screen capture (macOS): grab frames for a few seconds and report.
    CaptureTest {
        /// How many seconds to capture.
        #[arg(long, default_value_t = 3)]
        secs: u64,
        /// Target capture frame rate.
        #[arg(long, default_value_t = 30)]
        fps: u32,
    },
    /// Mirror this machine's screen to a DLNA device (live MPEG-TS over HTTP).
    Mirror {
        /// Target device id or address.
        device: String,
        /// Capture/encode frame rate.
        #[arg(long, default_value_t = 30)]
        fps: u32,
        /// Target video bitrate in kbps.
        #[arg(long, default_value_t = 8000)]
        bitrate_kbps: u32,
    },
    /// Cast a media file or URL to a device for native playback.
    Cast {
        /// Target device id or address.
        device: String,
        /// Path to a local media file or an http(s) URL.
        media: String,
    },
}

fn protocol_label(p: Protocol) -> &'static str {
    match p {
        Protocol::AirPlayMirror => "AirPlay (mirror)",
        Protocol::AirPlayVideo => "AirPlay (video)",
        Protocol::Dlna => "DLNA",
        Protocol::Miracast => "Miracast",
        Protocol::Cast => "Cast",
    }
}

/// Print a device list in a compact, human-readable form.
fn print_device_list(devices: &[Device]) {
    if devices.is_empty() {
        println!("No cast devices found on the local network.");
        return;
    }
    println!("Found {} device(s):", devices.len());
    for d in devices {
        let protos = d
            .protocols
            .iter()
            .map(|p| protocol_label(*p))
            .collect::<Vec<_>>()
            .join(", ");
        println!("  • {}  —  {}:{}  [{protos}]", d.name, d.address, d.port);
    }
}

/// Match a device by its id, exact name (case-insensitive), or IP address string.
fn find_device<'a>(devices: &'a [Device], needle: &str) -> Option<&'a Device> {
    devices.iter().find(|d| {
        d.id == needle || d.address.to_string() == needle || d.name.eq_ignore_ascii_case(needle)
    })
}

/// Cast a local media file to a DLNA device for native playback. Runs until Ctrl-C,
/// keeping the local media server alive for the duration.
async fn run_cast(device: &str, media: &str) {
    let is_url = media.starts_with("http://") || media.starts_with("https://");
    if !is_url && !PathBuf::from(media).is_file() {
        eprintln!("media not found (expected a local file path or an http(s) URL): {media}");
        return;
    }

    println!("Looking for '{device}'…");
    let devices = hc_discovery::discover_for(Duration::from_secs(5)).await;
    let Some(dev) = find_device(&devices, device) else {
        eprintln!("device '{device}' not found on the local network.");
        return;
    };
    let control_url = match resolve_dlna_control_url(dev).await {
        Some(url) => url,
        None => {
            eprintln!(
                "'{}' has no usable DLNA media endpoint (real-time mirroring is not implemented yet).",
                dev.name
            );
            return;
        }
    };

    let title = media_title(media);
    let result = if is_url {
        hc_sink::dlna::cast_url(&control_url, media, &title).await
    } else {
        hc_sink::dlna::cast_file(&control_url, dev.address, &PathBuf::from(media), &title).await
    };

    match result {
        Ok(session) => {
            println!(
                "▶ Casting '{title}' to {} ({})",
                dev.name,
                session.media_url()
            );
            println!("Press Ctrl-C to stop.");
            let _ = tokio::signal::ctrl_c().await;
            println!("Stopping…");
            if let Err(e) = session.stop().await {
                eprintln!("stop reported an error: {e}");
            }
        }
        Err(e) => eprintln!("cast failed: {e}"),
    }
}

/// Get the device's DLNA control URL, recovering it from the description document if
/// discovery didn't already resolve one.
async fn resolve_dlna_control_url(dev: &Device) -> Option<String> {
    if let Some(url) = dev.dlna_control_url.clone() {
        return Some(url);
    }
    let location = dev.dlna_location.clone()?;
    match hc_sink::dlna::resolve_control_url(&location).await {
        Ok(url) => url,
        Err(e) => {
            tracing::warn!(error = %e, "could not fetch device description for control URL");
            None
        }
    }
}

/// Live-mirror the screen to a DLNA device: capture+encode on a blocking thread, forward
/// encoded access units over a channel, and feed them to the live-mirror session until
/// Ctrl-C. (Capture+encode are macOS-only for now; on other platforms this reports it.)
async fn run_mirror(device: &str, fps: u32, bitrate_kbps: u32) {
    println!("Looking for '{device}'…");
    let devices = hc_discovery::discover_for(Duration::from_secs(5)).await;
    let Some(dev) = find_device(&devices, device) else {
        eprintln!("device '{device}' not found on the local network.");
        return;
    };
    let Some(control_url) = resolve_dlna_control_url(dev).await else {
        eprintln!(
            "'{}' has no DLNA media endpoint to mirror to (AirPlay-only devices aren't supported).",
            dev.name
        );
        return;
    };
    let (device_addr, dev_name) = (dev.address, dev.name.clone());

    let mut session =
        match hc_sink::dlna::LiveMirrorSession::start(&control_url, device_addr, "HorizonCast")
            .await
        {
            Ok(s) => s,
            Err(e) => {
                eprintln!("could not start live mirror: {e}");
                return;
            }
        };
    println!(
        "▶ Mirroring to {dev_name} ({}). Expect a few seconds of latency. Press Ctrl-C to stop.",
        session.stream_url()
    );

    let (tx, mut rx) = tokio::sync::mpsc::channel::<hc_encode::EncodedFrame>(120);
    let capture = tokio::task::spawn_blocking(move || capture_encode_loop(fps, bitrate_kbps, &tx));

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            unit = rx.recv() => match unit {
                Some(u) => session.push_access_unit(&u.data, u.pts, u.keyframe),
                None => { eprintln!("capture/encode ended."); break; }
            },
        }
    }

    println!("Stopping…");
    drop(rx); // makes the capture loop's next send fail, so it stops
    if let Err(e) = session.stop().await {
        eprintln!("stop reported an error: {e}");
    }
    let _ = capture.await;
}

/// Blocking capture→encode loop: pulls NV12 frames, encodes H.264, and sends Annex-B
/// access units to `tx`. Stops when capture ends or the receiver is dropped.
fn capture_encode_loop(
    fps: u32,
    bitrate_kbps: u32,
    tx: &tokio::sync::mpsc::Sender<hc_encode::EncodedFrame>,
) {
    let mut capture = match hc_capture::ScreenCapture::start(fps) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("screen capture unavailable: {e}");
            return;
        }
    };
    // First frame establishes the encoder's dimensions.
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
            eprintln!("encoder init failed: {e}");
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
                eprintln!("encode error: {e}");
                break;
            }
        }
        frame = capture.next_frame();
    }
    let _ = encoder.finish();
    capture.stop();
}

/// Probe the screen-capture backend: capture frames for `secs` seconds and report how
/// many arrived and at what resolution. Capture is blocking, so it runs on a blocking task.
async fn run_capture_test(secs: u64, fps: u32) {
    println!(
        "Starting screen capture at ~{fps} fps for {secs}s (grant Screen Recording if asked)…"
    );
    let outcome = tokio::task::spawn_blocking(move || {
        let mut capture = hc_capture::ScreenCapture::start(fps).map_err(|e| e.to_string())?;
        let deadline = std::time::Instant::now() + Duration::from_secs(secs);
        let mut frames = 0u32;
        let mut dims = (0u32, 0u32);
        while std::time::Instant::now() < deadline {
            match capture.next_frame() {
                Some(f) => {
                    frames += 1;
                    dims = (f.width, f.height);
                }
                None => break,
            }
        }
        capture.stop();
        Ok::<_, String>((frames, dims))
    })
    .await;

    match outcome {
        Ok(Ok((frames, (w, h)))) => {
            println!(
                "✓ Captured {frames} frames at {w}x{h} in {secs}s (~{} fps).",
                frames as u64 / secs.max(1)
            );
        }
        Ok(Err(e)) => eprintln!("capture failed: {e}"),
        Err(e) => eprintln!("capture task error: {e}"),
    }
}

/// Derive a display title from a file path or URL: last path segment, without extension.
fn media_title(media: &str) -> String {
    let path = media.split(['?', '#']).next().unwrap_or(media);
    let last = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let stem = last.rsplit_once('.').map_or(last, |(s, _)| s);
    if stem.is_empty() {
        "HorizonCast".to_string()
    } else {
        stem.to_string()
    }
}

#[tokio::main]
async fn main() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let cli = Cli::parse();
    match cli.command {
        Command::Devices { secs } => {
            tracing::info!("scanning for cast devices for {secs}s…");
            let devices = hc_discovery::discover_for(Duration::from_secs(secs)).await;
            print_device_list(&devices);
        }
        Command::Watch => {
            let discovery = hc_discovery::Discovery::start();
            let mut rx = discovery.subscribe();
            println!("Watching for cast devices (Ctrl-C to stop)…");
            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => break,
                    changed = rx.changed() => {
                        if changed.is_err() {
                            break; // service stopped
                        }
                        let devices = rx.borrow_and_update().clone();
                        println!("\n— device list updated —");
                        print_device_list(&devices);
                    }
                }
            }
            println!("\nStopping…");
            discovery.stop().await;
        }
        Command::CaptureTest { secs, fps } => {
            run_capture_test(secs, fps).await;
        }
        Command::Mirror {
            device,
            fps,
            bitrate_kbps,
        } => {
            run_mirror(&device, fps, bitrate_kbps).await;
        }
        Command::Cast { device, media } => {
            run_cast(&device, &media).await;
        }
    }
}

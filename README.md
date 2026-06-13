<center><img src="./assets/banner.png" alt="HorizonCast" /></center><br />

# HorizonCast

Cast your screen and system audio to any smart TV, with no app to install on the TV.

> 🚧 Early development.

## What it does

HorizonCast turns your Mac, Windows, or Linux machine into a caster for DLNA smart TVs.
Pick a TV on your network, then either mirror your screen live or hand the TV a video to
play. Built in Rust for low latency and smooth playback.

## Features

- **Live screen mirror** with system audio, hardware encoded (VideoToolbox on macOS, Media Foundation on Windows).
- **Cast a video file** that the TV plays natively at full quality.
- **Cast a link** by pointing the TV straight at a video URL.
- **Automatic discovery** of TVs on your network (mDNS and SSDP).
- **Quality controls** up to 4K, with a system audio toggle.
- **No receiver app** needed on the TV.
- **Cross platform**: macOS, Windows, Linux.

## How to cast

1. Open HorizonCast and pick your TV from the list.
2. Press **Cast** and choose a mode: Mirror, Video file, or Link.
3. Press **Stop casting** when you are done.

## Build

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Acknowledgments

The desktop GUI is made with [Slint](https://slint.dev) (used under the Slint Royalty-free License).

## License

MIT. See [LICENSE](LICENSE).

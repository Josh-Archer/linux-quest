# Contributing to linux-quest

We welcome contributions from systems engineers, graphics programmers, and VR enthusiasts!

## Development Philosophy
- **Performance First:** Every microsecond counts. Avoid heap allocations in hot frame loops, enforce zero-copy memory transfers, and measure latency regressions rigorously.
- **Modern Rust:** Idiomatic, safe Rust wherever possible, with carefully audited unsafe blocks for low-level FFI (Vulkan, DMA-BUF, NVENC, OpenXR).
- **Clean Architecture:** Keep crates modular and testable in isolation.

## Workspace Layout
- `crates/linux-quest-protocol`: Shared network packet definitions, binary serialization, telemetry formats.
- `crates/linux-quest-host`: The Linux desktop daemon orchestrating capture, encoding, and streaming.
- `crates/linux-quest-capture`: Screen capture engines (PipeWire, DMA-BUF, X11).
- `crates/linux-quest-encoder`: Hardware accelerated encoders (NVENC, VA-API).
- `crates/linux-quest-transport`: USB ADB reverse tunnel and low-latency UDP/QUIC network transport.
- `crates/linux-quest-input`: Linux kernel `/dev/uinput` injection.
- `client/quest-client`: Meta Quest 3 Android / OpenXR client.

## Submitting Pull Requests
1. Check existing [GitHub Issues](https://github.com/Josh-Archer/linux-quest/issues) to avoid duplicate work.
2. Ensure `cargo fmt` and `cargo clippy --all-targets` pass without warnings.
3. Include benchmarks or latency measurements when proposing optimizations in the hot path.

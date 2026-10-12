# Notes for agents

- `mod_audio_fork` is Rust: the core is `audio_fork` (no FreeSWITCH types, testing T1), the glue is the `mod_audio_fork` cdylib, the FFI is the `freeswitch` crate. Design and decisions: [docs/rust-rebuild-plan.md](docs/rust-rebuild-plan.md); tests: [docs/testing-plan.md](docs/testing-plan.md).
- Build and test: `just test` (fmt, clippy, unit and simulation tests) and `just test-fs` (real FreeSWITCH), both in Docker. `just test-load` is manual.
- Locally, `cargo test -p audio_fork` works without FreeSWITCH; the other crates need `just ide-setup` first (and only check, they do not link on macOS).
- Open review findings and verification gaps: [issues/review-findings.md](issues/review-findings.md).
- Periodically check the upstream repositories listed in [issues/upstream-watch.md](issues/upstream-watch.md) for protocol-level changes worth taking, and update that file when you do. If the user asks about upstream changes, start there.

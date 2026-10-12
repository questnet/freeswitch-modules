# Testing the Rust mod_audio_fork

How the Rust `mod_audio_fork` ([rust-rebuild-plan.md](rust-rebuild-plan.md)) is simulated and tested: without FreeSWITCH for timing and races, against a real FreeSWITCH for the FFI glue and media path, and against the C module for behaviour that is only specified by its implementation. Decisions were settled on 2026-10-12.

## Decision ledger

| # | Decision | Reason | Settled by |
|---|---|---|---|
| T1 | The core is FreeSWITCH-free: a separate crate `audio_fork` talks to the host only through a narrow port and per-frame functions; `mod_audio_fork` (cdylib) is glue | Deterministic simulation of races and timing without emulating FreeSWITCH; the dependency graph enforces the boundary | user |
| T2 | Real-FreeSWITCH tier: a `freeswitch` process driven over ESL, with loopback calls and `uuid_audio_fork` exactly as the IVR sends it | Tests the shipped `.so`, load/unload included, through the same path as production | user |
| T3 | This repo's test image builds FreeSWITCH 1.11.3 with only the modules the tests need, cached as a Docker stage and in the GitHub Actions cache; the real-FS tier runs in CI | Self-contained; the internal fs-image registry is assumed unreachable from GitHub runners | user |
| T4 | Differential tests: real-FS scenarios run against the C module too and the traces are compared, until the C sources are deleted | Retires the "preserved by formula, not by measurement" risk | user |
| T5 | Audio in through a `loopback/` pair at L16: leg B plays a sample-counter pattern, the fork sits on leg A; playout is checked by recording leg B. Tone frequency/energy checks where resampling is involved | Sample-exact checks of order, gaps and drops where no resampling happens | auto |
| T6 | One scripted WebSocket test server (tokio-tungstenite), shared by the simulation and real-FS tiers | One place for server behaviour: delay, refuse, stall, abrupt close, send binary/JSON | auto |
| T7 | The simulation tier steps a virtual 20 ms media clock and runs tokio with paused time | Races (hangup during connect, stop during flush) replay deterministically | auto |
| T8 | Property tests for inbound byte-stream splitting and for argument disambiguation | Both are rules over all inputs, not examples | auto |
| T9 | A `test-hooks` cargo feature (never in release builds) adds API subcommands that force a panic in the media callback or the task | Proves per-fork panic containment (D4) against a real FreeSWITCH | auto |
| T10 | ESL client: [freeswitch-esl-tokio](https://crates.io/crates/freeswitch-esl-tokio) | Maintained, tokio-native | auto |
| T11 | Load tier: N loopback pairs over ESL, CPU per call and overruns, with the C module as baseline while it exists | Same harness as the real-FS tier | auto |
| T12 | A minimal FreeSWITCH config dir is checked in (`tests/fs-conf`): event_socket, loopback, sndfile, tone_stream, dptools, the module under test | Reproducible, no dependency on fs-config | auto |
| T13 | Resampling sits behind a `Resample` trait in the core; the glue implements it with `switch_resample_*` (D8), the simulation tier with a simple test resampler | Keeps D8 and T1 compatible; the real-FS tier covers the FreeSWITCH resampler | auto |

FreeSWITCH ships its own unit-test framework, *fst* (`src/include/test/switch_test.h`): C macros that boot libfreeswitch in-process and originate a `null/` channel through mod_loopback. It is not used here (C only, and `null/` feeds comfort noise rather than controllable audio), but T12 and T5 borrow its two ideas: a minimal config dir and loopback channels.

## Tiers

| Tier | Runs | What it proves | Command |
|---|---|---|---|
| Unit | `cargo test`, no FS | Pure rules: parsing, prebuffer thresholds, set-aside byte, saturating mix, queue limits (property tests where the rule spans all inputs) | `just test` |
| Simulation | `cargo test`, no FS | The core's state machine, socket task and media side under scripted servers and virtual time | `just test` |
| Real FS | Test image, FS process + ESL | FFI glue, media bug flags, codec/rates, events and headers, load/unload, panic containment | `just test-fs` |
| Differential | Same as real FS, run twice | The Rust module produces the same traces as the C module | `just test-fs` (both modules) |
| Load | Test image, on demand | CPU per call, overruns at steady state, compared with C | `just test-load` |

CI runs `just test` and `just test-fs` on every PR. `test-load` is manual.

## Core port (T1, T13)

The core never names a FreeSWITCH type. Logging goes through `tracing`; the glue installs a subscriber that forwards to `switch_log_printf`.

```rust
/// What the core needs from the host, keyed by session UUID so a hung-up call
/// simply makes the operation a no-op.
pub trait Host: Send + Sync + 'static {
    fn fire(&self, session: &SessionId, event: Event);
    fn break_playback(&self, session: &SessionId);
}

pub trait Resample: Send {
    fn process(&mut self, input: &[i16], output: &mut Vec<i16>);
}
```

`Fork::start(config, host, runtime) -> MediaSide` spawns the socket task and returns the side the media thread owns:

- `MediaSide::on_read(&mut self, samples: &[i16])` — capture frame at the codec rate.
- `MediaSide::on_write_replace(&mut self, frame: &mut [i16])` — mix playout into the outgoing frame.
- `MediaSide::close(self)` — the bug's CLOSE.

Control (`send_text`, `stop`) goes through a cloneable `ForkHandle`. The glue's only jobs are turning FreeSWITCH callbacks into these calls and implementing `Host` and `Resample`.

## Simulation tier

A test builds a `FakeHost` that records events and `break_playback` calls, starts a scripted server, and drives `MediaSide` from a `MediaClock` that delivers one frame per virtual 20 ms tick (`tokio::time::pause` + `advance`). Scenarios:

- **Connect order:** metadata first, then queued texts in submission order, then binary frames; nothing is sent before the handshake completes.
- **Text limits:** exactly 1 MB accepted, 1 MB + 1 rejected; the 1025th queued message and the 4 MB total rejected before connect.
- **Connect failure:** refused and timed-out connects fire one `connect_failed` whose body is valid JSON even for reasons with quotes; the media side keeps accepting frames without growing memory.
- **Server stall:** the server stops reading; the capture ring fills after 2 s, frames are dropped, `buffer_overrun` fires exactly once.
- **Abrupt close:** the server drops TCP; one `disconnect` event, no further sends, the media side stays usable until CLOSE.
- **Stop:** the final text goes out before the close frame; `stop` while connecting sends nothing, fires no event and closes as soon as the handshake ends, also when CLOSE only comes later.
- **Hangup during connect:** CLOSE before the handshake; the task cancels, the socket is closed, no event fires after CLOSE.
- **Inbound messages:** `json` carries the whole message, `error` / `disconnect` carry `data`, unknown types and the removed `transfer` / `playAudio` fire nothing, a message over 1 MB closes the connection with `disconnect`.
- **killAudio:** `kill_audio` fires, `break_playback` is called, playout already buffered is discarded mid-stream.
- **Playout:** binary is ignored without write-replace; the first release waits for the 160 ms threshold (`320 * factor * 4` samples), later ones for 80 ms; a contended playout lock passes the frame through and loses no buffered audio; `playback` mode never sends binary.
- **Panic in the task:** an injected panic fires `error` and tears down that fork only; a second fork on the same runtime keeps streaming.

Property tests (T8): any split of an inbound byte stream into frames yields the same sample sequence; any token list is parsed into the same `Command` the C rules would produce (expected values computed by a direct transcription of the C branch structure, kept in the test).

## Real-FS tier

### Image

One multi-stage `Dockerfile`, all stages cached (buildx with the GitHub Actions cache in CI):

1. `fs-build`: Debian trixie, FreeSWITCH 1.11.3 configured with a minimal `modules.conf` (event_socket, loopback, sndfile, tone_stream, dptools, commands, console, logfile), `make install`.
2. `c-module` (until the C sources are deleted): libwebsockets v5.0.0 and the C `mod_audio_fork.so` built against `fs-build`.
3. `rust`: rustup, libclang; `cargo fmt --check`, `clippy`, unit + simulation tests, release build of `mod_audio_fork.so` (with and without `test-hooks`).
4. `fs-test`: `fs-build` + both `.so` files + the `fs_tests` binary.

`just test-fs` runs `fs_tests` in `fs-test` once per module (`AUDIO_FORK_MODULE=rust|c`); each run points FreeSWITCH at a mod dir that holds only that variant.

### Harness (`fs_tests` crate, not a default workspace member)

- Spawns `freeswitch -nc -nonat -conf tests/fs-conf ...` per test binary run, waits for ESL, kills it at the end and fails the run if FreeSWITCH crashed or logged a CRIT the test did not expect.
- Per test: originates a `loopback/` pair at the requested rate with L16, gets both UUIDs, starts the scripted server on an ephemeral port, sends `uuid_audio_fork` over ESL, collects events filtered by UUID.
- Audio in: leg B plays a generated `.r8` / `.r16` whose samples are a running 16-bit counter (`playback` from the mod_sndfile raw format). Playout out: `record_session` on leg B.
- Assertions read a normalised trace: WS frames (kind, length, counter ranges), events (name, `Audio-Fork-Bugname`, `Unique-ID`, body), recorded samples.

### Scenarios

- **Load lifecycle:** load, unload, reload; unload with two live forks does not crash and closes both sockets.
- **Start rejections:** usage, bad mix type, rate not a multiple of 8000, rate > 64000, unknown UUID, duplicate bugname, metadata > 1 MB; each returns `-ERR`/`-USAGE` and leaves no bug (`uuid_buglist`).
- **Capture:** mono / mixed / stereo × 8k / 16k codec × 8k / 16k target. Without resampling the counter arrives gapless and in order; stereo puts leg A's read audio in channel 0. With resampling, the tone frequency is checked instead.
- **Two forks on one channel** with different bugnames stream independently; `stop` of one leaves the other.
- **Events:** every event carries channel data and the right `Audio-Fork-Bugname`.
- **Bidi playout:** the server streams a counter pattern at the bidi rate, the recording on leg B contains it after the prebuffer delay; `killAudio` stops it within one frame and interrupts a `playback` application running on leg A (`CF_BREAK`).
- **Playback mode:** server audio is played, no binary frames reach the server.
- **Hangup races:** hangup during connect, during streaming, and `stop` racing `uuid_kill`, each repeated 200 times; FreeSWITCH stays up, no socket left open (`ss -tn` inside the container), no event for a fork after its CLOSE.
- **Panic containment** (`test-hooks` build): a forced panic in the media callback of call 1 removes that fork and fires `error`; call 2 keeps streaming the counter without a gap.
- **Receive limit:** a 1 MB + 1 text frame from the server closes the connection with `disconnect`; FreeSWITCH memory stays flat.

### Differential comparison (T4)

Each scenario above that the C module supports is run against both modules and the normalised traces are compared:

- WS frames: identical sequence of kinds and text payloads; binary frames identical in counter content, frame boundaries may differ.
- Events: identical names, bodies and `Audio-Fork-Bugname` per scenario step.
- Playout recordings: identical samples after aligning the start; the prebuffer delay may differ by at most one frame.

Intended differences are listed in the harness as named exceptions so a new difference fails the run:

- `transfer` and unknown message types (C fires `transfer` / logs at ERROR).
- `connect_failed` body (C does not escape it).
- Messages over the receive limit (C truncates and splits them).
- Unload with live forks (C is not clean).
- Rejected start arguments the C parser accepts by accident, if the property tests find any.

## Load tier

`just test-load N` originates N loopback pairs, starts a fork on each against the scripted server in sink mode, runs 60 s, and reports FreeSWITCH CPU per call, `buffer_overrun` count (must be 0) and the playout delay distribution, for both modules side by side.

## Where it goes in the rebuild steps

- Step 1 builds the `fs-build` and `c-module` stages and an empty `fs_tests` harness that starts and stops FreeSWITCH.
- Steps 3–6 add their scenarios to the unit and simulation tiers first, then to the real-FS tier.
- Step 7 is the hangup-race, panic and load-lifecycle scenarios.
- Step 9 is the load tier.
- Step 10 deletes the `c-module` stage and the differential exceptions.

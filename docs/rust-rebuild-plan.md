# Rebuilding mod_audio_fork in Rust

Plan for replacing the C/C++ `mod_audio_fork` on branch `robustness-fixes` with a Rust implementation of the same name. Decisions were settled on 2026-10-12; the ledger below is the source of truth, the steps follow from it.

## Status (2026-10-12)

Steps 1–7 and 9 are done on branch `rust-rebuild`, and step 10 except deleting the C/C++ sources, `Makefile.am` and `test/` in `mod_audio_fork/` (nothing builds them any more); `just test` and `just test-fs` pass. Step 8 (fs-image) is postponed: fs-image keeps building the C module from the `fs-jambonz` commit it points to, so its submodule must not move to `rust-rebuild` before step 8 is done. Still to do by hand: the IVR conversation against audio-knife (step 5) and the `cr-901` check (step 8). Open items: [issues/review-findings.md](../issues/review-findings.md).

Load (step 9, 100 calls, both modules optimised): FreeSWITCH used 1.00 % CPU per call with the Rust module and 1.29 % with the C module (mean of 5 runs), no buffer overruns, the same playout delay; details in [performance.md](performance.md).

Where the implementation differs from this plan and [testing-plan.md](testing-plan.md):

- An extra crate `test_server` holds the scripted WebSocket server (T6), shared by both test tiers. The glue has a fourth file, `log.rs`, for the `tracing` subscriber.
- The registry maps `(uuid, bugname)` to the fork's control handle and bug pointer; no channel private data and no cancellation token. Unload removes the bugs; CLOSE ends the task.
- The glue tokenises with FreeSWITCH's own `switch_separate_string` (so `'...'` quoting works) and hands the tokens to `api.rs`.
- `Host` has a third method, `end`, which the task supervisor uses to remove a fork's bug after a panic.
- `stop` or CLOSE during the connect aborts the connection attempt; nothing is sent, not even the metadata. No event is queued after CLOSE; the events of all forks are fired by one task in queue order, so neither CLOSE nor the media thread waits for one.
- The simulation tier pushes frames by count instead of stepping tokio's paused clock (T7): with paused time, tokio advances the clock while waiting on real socket I/O, which fires timeouts in the middle of a localhost handshake. Races are made deterministic by holding the server's handshake.
- The differential comparison (T4) was done by running every real-FS scenario's assertions against both modules, with C-only outcomes allowed only through named exceptions, instead of recording and diffing traces. The C module behaved the same in every scenario except the exceptions listed in [review-findings.md](../issues/review-findings.md#compatibility-notes-intentional-d1-and-d13); the comparison and the C build were removed in step 10.
- The prebuffer thresholds are 160 ms and 80 ms of audio at matching rates (the `320 * factor * 4` / `* 2` formula counts samples), as in the C module.

## Decision ledger

| # | Decision | Reason | Settled by |
|---|---|---|---|
| D1 | Keep only the interface in use (see [Interface to preserve](#interface-to-preserve)) | The only callers are the IVR (`ivr/IVR.FreeSwitch/InboundClient.fs`) and audio-knife | user |
| D2 | A separate `freeswitch` crate in this repo: bindgen allowlisted to the functions this module calls, plus a thin safe layer | Owns the hot path; [fswtch](https://github.com/RustedBytes/fswtch) is too young and lacks channel private data and panic guards | user |
| D3 | Replace in place: same module name, C/C++ sources deleted | Simpler packaging; rollback means redeploying an older image | user |
| D4 | Panics are contained per fork: `catch_unwind` at every FFI entry and around each fork's task; a panic logs at CRIT, tears down that fork, fires `mod_audio_fork::error` | A panic must not drop every call on the host; unwinding is free on the non-panicking path, and since Rust 1.81 a panic escaping `extern "C"` aborts | user + fact |
| D5 | One Rust Docker image for `just test` / CI (FS 1.11.3 headers, libclang, `cargo test`, cdylib build); fs-image builder gets the same cargo step; libwebsockets, boost, speexdsp-dev and SIMD flags go | The repo is the `fs-jambonz` submodule of fs-image | auto |
| D6 | One multi-thread tokio runtime per module (created on load, shut down on unload), tokio-tungstenite for the WebSocket | Async I/O for many forks; matches audio-knife | auto |
| D7 | Capture: bounded lock-free SPSC ring (2 s at the target rate), the media thread drops frames when it is full. Playout: deliberately uncapped, the media thread only `try_lock`s it | The media thread never blocks; uncapped playout is a decision from `robustness-fixes` (long streamed playout) | auto |
| D8 | Resampling with FreeSWITCH's `switch_resample_*` (speex inside FreeSWITCH), behind the core's `Resample` trait | Already linked, same quality as today, no extra dependency; the trait keeps the core FreeSWITCH-free (testing T13) | auto |
| D9 | rustls for `wss://` | No OpenSSL dependency | auto |
| D10 | Bindings generated at build time against the headers being built against, not checked in | Cannot drift from the FreeSWITCH version | auto |
| D11 | Tests as designed in [testing-plan.md](testing-plan.md): unit, simulation (no FreeSWITCH), real FreeSWITCH over ESL, differential against the C module, load | Settled separately (T1–T13) | user |
| D12 | The rewrite closes the open items of `issues/review-findings.md` by construction (bounded receive buffer, reserved/freed subclasses, escaped `connect_failed` JSON, clean unload with live forks, unknown message types not at ERROR); the file is then rewritten for the Rust code, `issues/upstream-watch.md` stays for protocol-level ideas | They are artifacts of the C design | auto |
| D13 | Drop the `transfer` message and event | No consumer; it only relays the server's request as an event, the module never transfers | user |
| — | Out of scope: remote `hangup` message, lower prebuffer (both from `upstream-watch.md`) | Features, not part of the port | auto |

## Interface to preserve

Anything not listed here is dropped: `pause`, `resume`, `graceful-shutdown`, `stop_play`, `transfer`, all `MOD_AUDIO_FORK_*` environment variables, the SIMD paths and `base64.hpp`. `playAudio` and `transcription` were already removed on `robustness-fixes`.

### API command `uuid_audio_fork`

Arguments are split on single spaces (`switch_separate_string` semantics, at most 10 tokens). JSON with spaces in it therefore breaks; audio-knife already copes with both encodings, so this stays.

- `<uuid> start <url> <mix> <rate> [...]`, at least 5 tokens:
  - `mix`: `mono` (read stream), `mixed` (+ write stream), `stereo` (+ write stream, stereo).
  - `rate`: `8k`, `16k` or an integer; must be > 0, a multiple of 8000, at most 64000.
  - 6 tokens: token 5 is metadata if it starts with `{` or `[`, otherwise the bugname.
  - 7–9 tokens: token 5 bugname, token 6 metadata.
  - 10 tokens: token 5 bugname and token 6 metadata (each ignored when empty), token 7 `true` / `playback` / other (bidi enable), token 8 `true` (bidi stream), token 9 bidi sample rate (0..=64000).
  - Write-replace (playout) is attached only when enable, stream and bidi rate are all set. `playback` additionally removes the read stream: server audio is played, caller audio is not forked. Unlike the C module, `playback` with `mixed` or `stereo` is rejected, as it would capture only the write stream.
  - Bidi enable defaults to off for fewer than 10 tokens.
  - The channel is pre-answered first; start fails if that fails, if the read codec is missing, if a bug with that name is already attached, if the URL does not parse, or if the metadata exceeds 1 MB.
- `<uuid> stop [bugname] [text]`: with 3 tokens, token 2 is the text if it starts with `{`/`[`, otherwise the bugname. The text is sent as the final frame before closing.
- `<uuid> send_text [bugname] <text>`: same disambiguation. One text frame per call, at most 1 MB; texts sent before the socket is connected are queued (max 1024 messages, 4 MB total) and flushed after the metadata. Over-limit is an error, never truncation.
- Replies: `+OK Success`, `-ERR Operation Failed`, `-USAGE: ...`.
- The default bugname is `audio_fork`.

### WebSocket

- Subprotocol `audio.drachtio.org`. `ws://` and `wss://`.
- On connect: metadata (if any) as one text frame, then queued texts, then capture audio as binary L16 little-endian frames at the requested rate, interleaved for stereo.
- Inbound text (JSON with `type`):
  - `killAudio`: fire `kill_audio` (no body), set `CF_BREAK` (value 2) on the channel, clear the playout buffer.
  - `disconnect`, `error`: fire the event with `data` serialised as body.
  - `json`: fire `json` with the **whole message** as body.
  - Anything else: logged at DEBUG/INFO, ignored.
- Inbound binary is accepted only when write-replace is attached; otherwise ignored.

### Events (`SWITCH_EVENT_CUSTOM`)

`mod_audio_fork::connect`, `::connect_failed` (body: JSON with the reason, properly escaped), `::disconnect`, `::error`, `::json`, `::kill_audio`, `::buffer_overrun`. Every event carries the channel data and an `Audio-Fork-Bugname` header. All subclasses are reserved on load and freed on unload.

### Audio behaviour

- Capture buffer: 2 s at the target rate. On overrun, frames are dropped and `buffer_overrun` fires once per fork.
- Bidi playout: inbound bytes are joined across frames (an odd trailing byte is set aside for the next frame), collected in a prebuffer, and released once it holds `320 * factor * 4` samples, after which the threshold drops to `320 * factor * 2` (`factor = bidi_rate / codec_rate`). Only whole multiples of `factor` samples are resampled per release; the remainder stays in the prebuffer. Resampled audio goes to the uncapped playout buffer.
- The write-replace callback mixes playout samples into the outgoing frame with saturating addition. When the playout buffer is cleared or the try-lock misses, the frame passes through untouched.

## Architecture

```
freeswitch-modules/
  Cargo.toml              workspace
  freeswitch/             bindings + thin safe layer (D2)
    build.rs              bindgen, allowlist, FS include dir from FREESWITCH_INCLUDE_DIR
    src/lib.rs
  audio_fork/             core, no FreeSWITCH dependency (testing T1)
    src/api.rs            parsing into a Command value (pure)
    src/fork.rs           Fork, MediaSide, ForkHandle; Host and Resample ports
    src/socket.rs         tokio task: connect, send, receive
    src/playout.rs        prebuffer, set-aside byte, resample chunking, mix (pure)
  mod_audio_fork/         cdylib glue, exports mod_audio_fork_module_interface
    src/lib.rs            module load/shutdown, runtime, registry, API entry
    src/bug.rs            media bug callback -> MediaSide
    src/host.rs           Host (events, CF_BREAK) and Resample (switch_resample) implementations
  fs_tests/               real-FreeSWITCH harness, not a default workspace member
  tests/fs-conf/          minimal FreeSWITCH config for fs_tests
```

The port between `audio_fork` and the glue is described in [testing-plan.md](testing-plan.md#core-port-t1-t13).

### `freeswitch` crate surface

Only what this module calls. Several FreeSWITCH entry points are macros around `*_perform_*` / `*_detailed` functions that take file/func/line, so the safe layer calls those directly:

- Module: the `switch_loadable_module_function_table_t` export, `switch_loadable_module_create_module_interface`, `switch_loadable_module_create_interface` (API interface), `switch_stream_handle_t::write_function`.
- Sessions: `switch_core_session_perform_locate` + `switch_core_session_rwunlock` (an RAII guard), `switch_core_session_get_channel`, `switch_core_session_get_read_codec`, `switch_core_session_get_read_impl`, `switch_channel_perform_pre_answer`, `switch_channel_set_private` / `get_private`, `switch_channel_set_flag_value`.
- Media bugs: `switch_core_media_bug_add`, `_remove`, `_get_session`, `_get_user_data`, `_read`, `_get_write_replace_frame`, `_set_write_replace_frame`.
- Events: `switch_event_create_subclass_detailed`, `switch_channel_event_set_data`, `switch_event_add_header_string`, `switch_event_add_body`, `switch_event_fire_detailed`, `switch_event_reserve_subclass_detailed`, `switch_event_free_subclass_detailed`.
- Resampling: `switch_resample_perform_create`, `switch_resample_process`, `switch_resample_destroy`.
- Logging: `switch_log_printf`, behind a small macro.

Every `extern "C"` callback the layer defines (API function, media bug callback, load/shutdown) is wrapped in `catch_unwind`.

### Ownership and threads

- The media bug's user data is a `Box<MediaSide>` handed to FreeSWITCH in `start` and reclaimed in `SWITCH_ABC_TYPE_CLOSE`. FreeSWITCH guarantees CLOSE runs exactly once, so this is the single teardown point; `stop` only removes the bug.
- `MediaSide` holds the capture producer, the playout buffer (`Arc<Mutex<VecDeque<i16>>>` plus an `AtomicBool` clear flag), a control sender to the task (texts, close-with-final-text), a `Notify` to wake the task after each pushed frame, and the overrun-notified flag.
- The task owns the capture consumer and the socket. It never holds a session pointer: to fire an event or set `CF_BREAK` it locates the session by UUID, so a hung-up call simply makes the lookup fail. This removes the class of lifetime races the C code fixed piecemeal.
- A global registry (`Mutex<HashMap<(uuid, bugname), CancellationToken>>`) is touched only on start, close and unload, never per frame. On unload, every registered fork's bug is removed (locate by UUID, `media_bug_remove`), then the runtime shuts down with a timeout. FreeSWITCH must not call into unmapped code after unload.
- A panic inside the task is observed by a supervisor `JoinHandle`, which fires `error` and cancels the fork. A panic in the media bug callback returns `SWITCH_FALSE`, so FreeSWITCH removes the bug and CLOSE runs.

## Steps

Each step ends with something that builds and is checked; commit after each.

1. **Scaffold and build pipeline.** Workspace with `freeswitch` (`build.rs`: bindgen over the allowlist), `audio_fork`, the empty cdylib and the `fs_tests` harness. Rewrite the `Dockerfile` into the stages of [testing-plan.md](testing-plan.md#image): `fs-build` (FreeSWITCH 1.11.3, minimal modules), `c-module`, `rust` (`fmt --check`, `clippy`, `cargo test`, release build), `fs-test`. `just test` and `just test-fs`; the CI matrix becomes those two with the buildx GitHub Actions cache; drop `test-sse2`; update `ide-setup` for rust-analyzer. *Check:* `just test` passes; `nm -D` on the `.so` shows `mod_audio_fork_module_interface`; `just test-fs` starts and stops FreeSWITCH with the C module loaded.
2. **Module skeleton.** Load/shutdown, subclass reservation, `uuid_audio_fork` returning `-USAGE`. *Check:* real-FS load lifecycle scenario (load, unload, reload, `-USAGE` reply).
3. **Argument parsing.** `api.rs` turns tokens into a `Command` (`Start { .. } | Stop { .. } | SendText { .. }`) with the rules above. *Check:* unit and property tests for the parsing rules; real-FS start-rejection scenario, differential against C.
4. **Capture path.** Media bug with read/write stream flags, resampling to the target rate, the capture ring, the socket task (connect, metadata, queued texts, binary frames, final stop text, close), events `connect` / `connect_failed` / `disconnect`, overrun policy. *Check:* simulation scenarios connect order, text limits, connect failure, server stall, abrupt close, stop; real-FS capture, two-forks and events scenarios, differential against C.
5. **Inbound messages.** JSON dispatch to `json` / `error` / `disconnect` / `kill_audio` (with `CF_BREAK`), `Audio-Fork-Bugname` header, receive limit via `max_message_size` (1 MB). *Check:* simulation inbound-message and killAudio scenarios; real-FS receive-limit scenario; one manual IVR conversation against audio-knife via `configuration/fs290` `cr-901`.
6. **Bidi playout.** `playout.rs` (set-aside byte, prebuffer thresholds, chunking by factor, resample), write-replace callback with saturating mix, `killAudio` clearing, `playback` mode. *Check:* unit and property tests for the odd-byte carry, thresholds and saturating mix; simulation playout scenarios; real-FS bidi playout and playback-mode scenarios, differential against C.
7. **Failure and lifecycle.** Panic containment at all entries (a debug-only API subcommand or test hook that panics, to prove the fork dies and other calls survive), hangup during connect, `stop` racing hangup, unload with live forks. *Check:* simulation hangup-during-connect and task-panic scenarios; real-FS hangup races, panic containment (`test-hooks`) and unload with live forks.
8. **fs-image integration.** Replace the libwebsockets/autotools steps for this module with the cargo build and install the `.so` into the mod dir; remove libwebsockets from the runtime image. *Check:* fs-image tests pass, `cr-901` works in the composed setup.
9. **Load check.** `just test-load` ([testing-plan.md](testing-plan.md#load-tier)): CPU per call, playout delay, no buffer overruns at steady state, side by side with the C module.
10. **Docs and cleanup.** Rewrite `mod_audio_fork/README.md` to the preserved interface, update the top-level README dev table and `AGENTS.md`, rewrite `issues/review-findings.md` for the Rust code, delete the C/C++ sources, `Makefile.am`, `test/`, the `c-module` stage and the differential exceptions, and the `scripts/ide-setup.sh` parts that no longer apply.

## Risks

- **Unload with live calls** has never been clean, in C or here: if FreeSWITCH still holds a bug whose callback lives in the unloaded library, it crashes. Step 7 must prove the unload path removes every bug first.
- **Callers outside our stack** relying on the dropped commands, `transfer`, or env vars lose them silently. Within questnet only the IVR and audio-knife were found (`~/dev/qn`, 2026-10-12).
- **bindgen in the image** needs libclang and the generated FS config headers; the header-prep trick from today's Dockerfile carries over, but a mismatch with the real build in fs-image would only show at load time. Step 8 builds against the exact fs-image tree.
- **Prebuffer behaviour** is preserved by formula; the differential playout comparison (testing T4) measures it against the C module.

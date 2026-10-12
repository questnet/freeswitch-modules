Findings for the Rust `mod_audio_fork` (crates `audio_fork`, `freeswitch`, `mod_audio_fork`). The findings on the C/C++ module it replaced are in git history (this file before the Rust rebuild).

## Closed by the rewrite (D12)

- **Receive buffer unbounded / oversized messages split** (C, Medium): tungstenite's `max_message_size` is 1 MB; a larger message closes the connection with `disconnect` (real-FS `receive_limit`).
- **Event subclasses not all reserved / freed** (C, Low): all seven are reserved on load and freed on unload, under one owner name.
- **`connect_failed` body concatenated by hand** (C, Low): built with `serde_json` (unit test with quotes, newlines, backslashes).
- **Unload with live forks not clean** (C, Medium): unload removes every attached bug first, then shuts the runtime down (real-FS `unload_with_live_forks`).
- **Unknown message types logged at ERROR** (C, Low): logged at INFO.
- **Use-after-free and lifetime races between the lws thread and session cleanup** (C, the bulk of the `robustness-fixes` work): the socket task never holds a session; it reaches FreeSWITCH by UUID, so a hung-up call makes it a no-op. The bug's user data is reclaimed only in CLOSE, which FreeSWITCH runs exactly once.
- **Exceptions/panics escaping into C** (C): every `extern "C"` entry point and each fork's task contain panics (real-FS `panic_containment`).

## Open

- [x] **Unload racing a `start`** (`mod_audio_fork/src/lib.rs`, Low). A fork is registered before its bug is attached; unload skips registered forks whose bug is not attached yet and then drops the registry, so the `start` in flight panics on the missing registry after attaching a bug whose callback is about to be unmapped. Needs `uuid_audio_fork start` concurrently with `unload mod_audio_fork`. Fix: have unload wait for starts in flight, or refuse new starts once unload has begun.
- [ ] **No TCP keepalive on the WebSocket** (`audio_fork/src/socket.rs`, Low). The C module set lws keepalive (55 s, 4 probes, 5 s apart); here a half-open connection is only noticed when a send fails. The server's pings are answered.
- [x] **Events are fired under the fork's outbox lock** (`Shared::fire`, Low). This is what makes "no event after CLOSE" hold, but CLOSE (on the media thread) waits while an event is being created and queued. FreeSWITCH queues events without blocking in normal operation. Fixed: forks queue their events, one task fires them in queue order (`audio_fork/src/events.rs`); a fork queues nothing once CLOSE cancelled its token, and CLOSE cancels it before the registry entry goes.
- [x] **A text taken off the queue is lost to a CLOSE that came meanwhile** (Medium, `audio_fork/src/socket.rs` `send`). The send raced the `closed` token with the token first, so the final text of a `stop` could be dropped unsent; the simulation test `stop_sends_the_final_text_before_closing` failed about once in 30 runs once events were queued. Fixed: the send is tried first, the token only ends a send that stalls.
- [ ] **The tail of a playout stream waits for more audio** (`audio_fork/src/playout.rs`, behaviour kept from C). Audio under one release step (80 ms at matching rates, `320 * factor * 2` samples) stays in the prebuffer until more arrives, so the end of each streamed utterance is played late or, if nothing follows, not at all. Seen in the real-FS playout scenarios (the last 16 samples of a 2 s stream). Changing it is a behaviour change; decide together with the lower prebuffer from `upstream-watch.md`.
- [ ] **`mixed` and `stereo` capture send nothing while one direction is silent** (FreeSWITCH, behaviour kept from C). `switch_core_media_bug_read(..., fill = TRUE)` returns no frame while either the read or the write buffer is empty, so a caller who hears nothing (e.g. a parked leg) is not forked at all. `mono` is not affected.

## Code review 2026-10-12 (`/rust-code-review` of `1f62cc7..dfd42fc`)

Numbers as in the review.

- [x] **1.1 Unloading while a call is hanging up leaves a bug that calls into the unloaded library** (High, `mod_audio_fork/src/lib.rs` `shutdown`). Bugs are removed only for sessions `Session::locate` finds, and `switch_core_session_read_lock` refuses once the channel is down (`switch_core_rwlock.c:89`). The skipped bug's CLOSE comes after the registry and the runtime are gone: `expect("CLOSE after unload")` panics, and the callback runs in a library FreeSWITCH may have unmapped. Fixed: unload refuses new starts, removes the reachable bugs, then waits up to 10 s until CLOSE has emptied the registry (CRIT if not); a start that attached its bug meanwhile removes it itself. Covers "Unload racing a `start`" above. Real-FS scenario `unload_during_hangups` (logged `CLOSE after unload` three times out of three before).
- [x] **1.2 `killAudio` throws away the audio that follows it and leaves a stale tail** (Medium, `audio_fork/src/playout.rs`, `socket.rs`). The clear is deferred to the media thread's next frame, so audio appended in between is discarded with the killed audio; the prebuffer keeps the killed stream's remainder, which is played in front of the next utterance. Both inherited from C. Fixed: the task empties the playout buffer under the lock and drops what the prebuffer holds (the release threshold stays); simulation tests `audio_sent_right_after_kill_audio_is_played` and `kill_audio_drops_what_the_prebuffer_still_holds`.
- [x] **1.3 A `MediaSide` dropped without `close()` leaves its task running** (Medium, `audio_fork/src/fork.rs`). Two concurrent `start`s with the same bugname: the loser drops its `MediaSide`, its task connects, fires `connect` for a fork it does not own and holds the socket. Same for a panic in CLOSE after the user data is reclaimed. Fixed: `Drop for MediaSide` does what `close` did; simulation test `dropping_the_media_side_ends_the_fork`.
- [x] **1.4 Lifecycle in mirrored state** (Low, `fork.rs`). `Outbox.closed` mirrors the `closed` token; the struct named `Outbox` guards the lifecycle too. Fixed: the token is cancelled under the lock and tested there; the struct is `State`.
- [x] **1.5 `wss://` reloads the system certificates on every connect** (Low, `socket.rs`). tokio-tungstenite builds a rustls config per connection without a connector (`tls.rs:95-100`), blocking file I/O on a runtime worker. Fixed: one `ClientConfig` per process, passed as `Connector::Rustls` (still untested, see the verification gaps).
- [x] **1.6 The registry's "module is loaded" unwrap at every use** (Low, `mod_audio_fork/src/lib.rs`). Fixed: `registry()`.
- [x] **2.1–2.3 Comments**: `fork.rs` overrun comment contradicted by `panicked()`; `lib.rs` `closed()` comment claims the invariant 1.1 breaks; `api.rs` module doc contrasts with the removed C module.
- [x] **3.1 `hangup_during_connect_cancels_without_events` cannot fail for its stated reason** (`audio_fork/tests/simulation.rs`): it waits 300 ms against the 10 s default connect timeout.
- [x] **3.2 `task_panic_ends_that_fork_only` is racy** (`audio_fork/tests/panics.rs`): `end` is called after `error` fires, the test asserts it right after the event.
- [x] **3.3 `first_counter` underflows on a run starting at 0** (`fs_tests/tests/load.rs`).
- [x] **3.5 Missing tests**: `killAudio` followed by a new stream plays it from its first sample and without the old tail (1.2); unload with hangups in flight (1.1). Both added. The reworked 3.1 test also caught that the 1.5 connector read the system roots for `ws://` too, inside the connect timeout; fixed.

Found while fixing:

- [x] **Upper-case URL schemes failed to connect** (`audio_fork/src/api.rs`). The parser accepted `WS://` / `WSS://` (as the C module did), but tungstenite only connects to a lower-case scheme and fired `connect_failed` ("unsupported URL scheme"). Fixed: the parser lower-cases the scheme; unit test `schemes_are_lower_cased_for_tungstenite`.

## Code review 2026-10-12 (2) (`/rust-code-review` of `fa2cb1c..aa5d6a1`)

Numbers as in the review.

- [x] **1.1 A panic while firing an event stops the events of every fork** (Medium, `audio_fork/src/events.rs`). The queue's task calls `host.fire` without containing a panic, and `Session::fire_custom` asserts that the event was created. One panic ends the task; later events of all calls are lost without a log until the module is reloaded. Before the queue, the fork's own socket task panicked and `supervise` contained it (D4). Fixed: each fire is contained and a panic logged at CRIT, so it costs only that event; unit test `a_panicking_fire_loses_only_that_event`.
- [x] **1.2 A server flooding events was no longer slowed down** (Medium, `events.rs`). The socket task queued events without waiting, so a server sending messages faster than FreeSWITCH takes events grew the queue without bound. Before, FreeSWITCH blocked the task's fire once its dispatch queue was full (10000 events per dispatch thread; `switch_event.c:393`, `switch_apr_queue.c:148`, FreeSWITCH 1.11.3), and the task stopped reading the socket. Fixed: the queue holds 1024 events and the socket task waits for room; the media thread's `buffer_overrun` and panic `error` are queued by a spawned task, so they are neither dropped nor make the media thread wait.
- [x] **1.3 Events still queued at unload are lost** (Low, `mod_audio_fork/src/lib.rs` `shutdown`). Accepted: the runtime shuts down without draining the queue; the forks being unloaded are ending anyway. Documented in the README.
- [x] **1.4 "Queues nothing after CLOSE" has a window** (Low, `Shared::fire`). A CLOSE between the token check and the send lets one event through. Accepted: a new fork with the same bugname fires nothing before its handshake, which takes far longer. Documented at `Shared::fire`.
- [ ] **1.5 The media thread locks `State` for every frame** (Low, `fork.rs` `on_read`). The critical sections are short and never span a call into the host, but parking_lot has no priority inheritance. Check with `just test-load` against the last numbers (1.00 % CPU per call, no overruns).
- [ ] **2.1, 2.2 Comments**: the fix noted for 1.4 of the previous review (token cancelled under the lock) is superseded by the event queue; `docs/rust-rebuild-plan.md` still describes an outdated `MediaSide`.
- [ ] **3.2 Absence checks without a pause** (`audio_fork/tests/simulation.rs`, `refused_connect_fires_connect_failed_once` and `stop_sends_the_final_text_before_closing`): with queued events, a wrongly queued extra event can reach the fake host only after the assertion.

## Compatibility notes (intentional, D1 and D13)

Dropped compared to the C module: the subcommands `pause`, `resume`, `graceful-shutdown`, `stop_play`; the `transfer` message and event; all `MOD_AUDIO_FORK_*` environment variables; the channel variables `MOD_AUDIO_BASIC_AUTH_USERNAME`/`_PASSWORD` (basic auth) and `MOD_AUDIO_FORK_ALLOW_SELFSIGNED`, `_SKIP_SERVER_CERT_HOSTNAME_CHECK`, `_ALLOW_EXPIRED` (TLS checks are always on); `http://` and `https://` URLs. Rates must be plain integers (the C `atoi` accepted `8000x`). Malformed `start` arguments are answered with `-USAGE` where the C module sent no reply (ESL clients saw `-ERR no reply`). Within questnet only the IVR and audio-knife use the module, and neither uses any of these (checked 2026-10-12).

## Verification gaps

- `wss://` is not tested: no scenario uses TLS.
- Not built against fs-image's FreeSWITCH tree yet (rebuild step 8): the bindings are generated from the headers of this repo's test image, FreeSWITCH 1.11.3 built with a minimal module set.
- The manual IVR conversation against audio-knife (`configuration/fs290`, `cr-901`, rebuild step 5) has not been done.
- In the real-FS playout scenarios the first played frame does not appear in leg B's recording, with the C module as well; assumed to be the recording starting, not checked further.

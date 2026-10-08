Findings from a full read-through of `mod_audio_fork/` (C/C++ sources). Not reviewed: SSE2/scalar halves of `vector_math.cpp`, `base64.hpp` body, `test/test_units.cpp`, Dockerfile, justfile, docs.

Status is kept up to date as items are fixed. Items not touched since the original review are the review's claims and have not been re-verified against the current code.

## Fixed on branch `robustness-fixes`

- [x] Start with invalid URI/sample rate still starting the capture, `playAudio` NULL `audioContentType` crash, stereo+resampling buffer overflow, null derefs in error logs.
- [x] **Use-after-free on `AudioPipe`.** The pipe is owned by `shared_ptr` (session side via `PipeHandle`, pending lists, lws slot).
- [x] **Races between the lws thread and session cleanup.** Pre-buffer, set-aside byte, bidirectional resampler and playout buffer moved into a shared `StreamState` that the `AudioPipe` passes to every callback (`userData`), so cleanup only drops its reference and the lws thread never uses memory cleanup frees. The playout buffer is shared with the media thread under `StreamState::playoutMutex` (media thread only try-locks it).
- [x] **Concurrent `stop` and media-bug close.** `fork_session_cleanup` re-checks the bug under `tech_pvt->mutex`; only the first caller tears down. `tech_pvt->mutex` is no longer destroyed (session pool owns it) and is unlocked at the end of cleanup.
- [x] **`AudioPipe::context` / `stopFlag` data races.** Both atomic and documented in `audio_pipe.hpp`; `addPendingConnect` skips `lws_cancel_service` while the context does not exist yet, and the service thread wakes itself once it has created it.
- [x] **Removed `playAudio` and `transcription`** (messages, events, README). The `playAudio` temp-file list and its races went with it. `killAudio` is kept.
- [x] **Leaks / sockets left open.** `close()` on a pipe that is `IDLE` or `CONNECTING` is remembered (`m_closeRequested`) and honoured: an `IDLE` pipe never connects, a `CONNECTING` one is closed in `ESTABLISHED`. `connect_client` failing emits `CONNECT_FAIL` and drops the pipe from the pending list. `start_capture` destroys the session data when `media_bug_add` fails and runs the full cleanup when connecting fails. `fork_data_init` failure paths audited: `destroy_tech_pvt` is safe there. `CLIENT_CONNECTION_ERROR` already removed the pipe from the pending list.
- [x] **Unterminated strings.** `parse_ws_uri` no longer copies the URI into a fixed buffer, rejects hosts/paths that do not fit and copies with `switch_copy_string`; the caller's buffers are zero-initialised. `sessionId` is a session-pool string like `bugname`, and the unused `host`/`path`/`port` fields of `private_t` are gone.
- [x] **Unbounded memory growth on inbound binary.** The pipe accepts binary only if enable, stream and sample rate are all set (the condition for `SMBF_WRITE_REPLACE`); `enable` now defaults to 0. The playout buffer is deliberately left uncapped (long streamed playout is wanted). Not built or run.
- [x] **Mixing wrapped instead of clipping.** `vector_add` now saturates in the AVX2, SSE2 and scalar paths (`adds_epi16` / clamped scalar add); unit test added and passing for scalar and SSE2. The AVX2 path is not built by the test targets.
- [x] **Module unload could hang.** `deinitialize` now calls `lws_cancel_service` after setting `stopFlag` (if the context does not exist yet the service thread wakes itself and sees the flag), and the service thread resets `context` to null before `lws_context_destroy`. Not built or run. Still open: `fork_cleanup` does not close live pipes, so unloading with calls in progress is not clean.

## Medium

- [ ] **Graceful shutdown blocks closing.** After `graceful-shutdown` the WRITEABLE handler returns early on `isGracefulShutdown()` and never reaches the `DISCONNECTING` branch, so a later `stop` cannot close our side. Also logged at ERROR for a normal event.

- [x] **Unlocked `PipeHandle::get` in API calls** (`lws_glue.cpp`, from `/code-review`). `fork_session_send_text` and `fork_session_graceful_shutdown` read `tech_pvt->pAudioPipe` without `tech_pvt->mutex`, while cleanup deletes the heap `Ptr` box in `PipeHandle::release` under it, so a concurrent `stop`/hangup could leave them dereferencing a freed box (narrow window). Both now take `tech_pvt->mutex` around `get` and the call. `fork_session_pauseresume` was named in the review but never uses `PipeHandle::get`. Not built or run.
- [ ] **`close()` vs `LWS_CALLBACK_CLIENT_ESTABLISHED` race** (`audio_pipe.cpp` ~119, unverified). The `m_closeRequested` path sets `DISCONNECTING` and returns -1, while `close()` can concurrently CAS `CONNECTED` to `DISCONNECTING` and queue a second disconnect for a wsi that is already closing. Text queued before establishment (initial metadata, final stop text) is dropped without a log line.
- [ ] **Short `lws_write` on a text frame closes the connection** (`audio_pipe.cpp` ~262, unverified). The code returns -1 although the comment says lws buffers short writes; with messages up to 1 MB (`MAX_TEXT_LEN`) this becomes likely and the dequeued message is lost.
- [ ] **`deinitialize` does not drain the static `pendingConnects` / `pendingDisconnects` / `pendingWrites`** (`audio_pipe.cpp` ~524, unverified). They own `shared_ptr`s to pipes whose `userData` holds `StreamState`; destructors (incl. `speex_resampler_destroy`) can run after `dlclose`. Related to the open `fork_cleanup` item above.
- [ ] **`dub_speech_frame` skips the whole frame when `clearPlayout` is set** (`lws_glue.cpp` ~1471, unverified) instead of clearing the buffer and still going through the write-replace path, and drops buffered playout when the try-locks fail (a frame of delay, audible gaps).
- [ ] **`fork_session_cleanup` returns early without `destroy_tech_pvt` when the bug private is missing** (unverified; the `tech_pvt->id` read before the null check is listed under Low). Pipe and stream references are then not released.
- [ ] **`connect_client` failure path** (`audio_pipe.cpp` ~399, unverified): between removal from `pendingConnects` and setting `FAILED`, `close()` sees `CONNECTING` and does nothing, and `CONNECT_FAIL` can be dispatched for an already torn-down session. Overlaps with the "Leaks / sockets left open" fix above, so check whether that fix already covers it.
- [ ] **Possible leak when `fork_data_init` fails** (`lws_glue.cpp` ~336, unverified, **disputed**): the review says `PipeHandle::set` / `StreamHandle::set` boxes leak when the resampler init fails and `start_capture` does not clean up. The "Fixed" list above claims `destroy_tech_pvt` is safe on those paths and `start_capture` runs the full cleanup, so re-check before acting.

## Low

- `enqueueText` recomputes the queued byte total by iterating the deque under `m_text_mutex` on every call (up to 1024 entries); keep a running counter instead (`audio_pipe.cpp` ~583, from `/code-review`).
- `binaryWritePtrResetToZero` resets to 0 instead of `LWS_PRE`, so after an overrun the first 16 bytes are skipped; `buffer_overrun_notified` is never reset.
- `fork_session_cleanup` reads `tech_pvt->id` before the `!tech_pvt` check.
- `CONNECT_FAIL` JSON is concatenated by hand: message not escaped and may be NULL. Use cJSON.
- Basic auth is silently skipped if user+password exceed the 128-byte buffer, and is sent over plain `ws://`.
- `EVENT_CONNECT_SUCCESS`, `CONNECT_FAIL`, `BUFFER_OVERRUN`, `JSON` are never reserved/freed; `switch_event_create_subclass` result unchecked in `responseHandler`.
- Remaining API error logs use `session` (NULL in API context) instead of `lsession`; `parser.cpp` logs full inbound messages at ERROR.
- Metadata containing spaces is split by `switch_separate_string(' ')`, breaking the JSON.
- No `realloc` failure handling for `recv_buf` growth.
- Samples not consumed by the resampler (`in_len`) are dropped by `cBuffer->clear()`.
- Unknown message types (including the removed `playAudio` / `transcription`) are logged at ERROR.

## Compatibility notes (intentional, from `/code-review`)

- `bidirectional_audio_enable` now defaults to 0 for the short start syntax (`argc <= 9`), and `playAudio` / `transcription` messages and their events (`play_audio`, `transcription`) were removed. Existing jambonz/FreeSWITCH callers relying on them silently lose those events. Call this out in the PR description and release notes.

## Cleanups

- Dead code: `nServiceThreads`, `basicAuthUser`/`basicAuthPassword`, `fork_service_threads`, `mod_audio_fork_runtime`, `!ap` after `new`, unused `sent`, and the other unused-variable warnings from the build.
- `bidirectional_audio_enable` / `bidirectional_audio_stream` in `private_t` are now written but never read.
- `base64.hpp` and its unit test are unused now.
- `dub_speech_frame` holds `playoutMutex` while mixing; copy the samples out under the lock and mix outside to shorten it.

## Verification gaps

- No ThreadSanitizer run and no test of the shared-state / callback path (`AudioPipe` needs FreeSWITCH + lws to build).
- No live-call run of the race fixes.
- The string-termination fix is only reviewed, not built or run (`parse_ws_uri` needs FreeSWITCH).

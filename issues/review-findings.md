Findings from a full read-through of `mod_audio_fork/` (C/C++ sources). Not reviewed: SSE2/scalar halves of `vector_math.cpp`, `base64.hpp` body, `test/test_units.cpp`, Dockerfile, justfile, docs.

Status is kept up to date as items are fixed. Items not touched since the original review are the review's claims and have not been re-verified against the current code.

## Fixed on branch `robustness-fixes`

- [x] Start with invalid URI/sample rate still starting the capture, `playAudio` NULL `audioContentType` crash, stereo+resampling buffer overflow, null derefs in error logs.
- [x] **Use-after-free on `AudioPipe`.** The pipe is owned by `shared_ptr` (session side via `PipeHandle`, pending lists, lws slot).
- [x] **Races between the lws thread and session cleanup.** Pre-buffer, set-aside byte, bidirectional resampler and playout buffer moved into a shared `StreamState` that the `AudioPipe` passes to every callback (`userData`), so cleanup only drops its reference and the lws thread never uses memory cleanup frees. The playout buffer is shared with the media thread under `StreamState::playoutMutex` (media thread only try-locks it).
- [x] **Concurrent `stop` and media-bug close.** `fork_session_cleanup` re-checks the bug under `tech_pvt->mutex`; only the first caller tears down. `tech_pvt->mutex` is no longer destroyed (session pool owns it) and is unlocked at the end of cleanup.
- [x] **`AudioPipe::context` / `stopFlag` data races.** Both atomic and documented in `audio_pipe.hpp`; `addPendingConnect` skips `lws_cancel_service` while the context does not exist yet, and the service thread wakes itself once it has created it.
- [x] **Removed `playAudio` and `transcription`** (messages, events, README). The `playAudio` temp-file list and its races went with it. `killAudio` is kept.

## Medium

- [ ] **Leaks / sockets left open.** `AudioPipe::close()` is a no-op unless `CONNECTED`, so `stop` during `CONNECTING` leaves the connection to complete with nobody to close it. `connect_client` returning false emits no `CONNECT_FAIL`. `fork_data_init` failures after pipe creation and a failed `switch_core_media_bug_add` in `start_capture` leak the pipe and `tech_pvt` resources. "`CLIENT_CONNECTION_ERROR` never deletes the pipe" may be gone with the shared_ptr ownership; re-check.
- [ ] **Graceful shutdown blocks closing.** After `graceful-shutdown` the WRITEABLE handler returns early on `isGracefulShutdown()` and never reaches the `DISCONNECTING` branch, so a later `stop` cannot close our side. Also logged at ERROR for a normal event.
- [ ] **Module unload can hang.** `deinitialize` sets `stopFlag` and joins but never calls `lws_cancel_service`; the thread blocks in `lws_service(context, 0)`. `fork_cleanup` does not close live pipes. `context` is not reset after `lws_context_destroy`.
- [ ] **Unbounded memory growth on inbound binary.** `bidirectional_audio_stream_enable = enable + stream` (`lws_glue.cpp`) with `enable` defaulting to 1 makes `is_bidirectional_audio_stream()` true by default even though `SMBF_WRITE_REPLACE` is not set; binary frames then fill a playout buffer nothing drains. Even in stream mode both buffers grow via `set_capacity` with no cap. With `playAudio` gone, "enable without stream" has no purpose any more and could be dropped.
- [ ] **Mixing wraps instead of clipping.** `vector_add` uses wrapping `_mm256_add_epi16` (and `a[i] += b[i]` in the tail); `vector_normalize` then clamps already-wrapped values and is effectively a no-op. Use saturating adds (`_mm256_adds_epi16`). Only the AVX2 path and scalar tail were checked.
- [ ] **Unterminated strings.** `parse_ws_uri` does `strncpy(host, ..., MAX_WS_URL_LEN)` into an uninitialised `host[512]`; a host of 512+ chars has no terminator. Same for `sessionId` and other `strncpy` calls in `fork_data_init`.

## Low

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

## Cleanups

- Dead code: `nServiceThreads`, `basicAuthUser`/`basicAuthPassword`, `fork_service_threads`, `mod_audio_fork_runtime`, `!ap` after `new`, unused `sent`, and the other unused-variable warnings from the build.
- `bidirectional_audio_enable` / `bidirectional_audio_stream` in `private_t` are now written but never read.
- `base64.hpp` and its unit test are unused now.
- `dub_speech_frame` holds `playoutMutex` while mixing; copy the samples out under the lock and mix outside to shorten it.

## Verification gaps

- No ThreadSanitizer run and no test of the shared-state / callback path (`AudioPipe` needs FreeSWITCH + lws to build).
- No live-call run of the race fixes.

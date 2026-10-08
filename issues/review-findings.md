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
- [x] **Unlocked `PipeHandle::get` in API calls** (`lws_glue.cpp`, from `/code-review`). `fork_session_send_text` and `fork_session_graceful_shutdown` read `tech_pvt->pAudioPipe` without `tech_pvt->mutex`, while cleanup deletes the heap `Ptr` box in `PipeHandle::release` under it, so a concurrent `stop`/hangup could leave them dereferencing a freed box (narrow window). Both now take `tech_pvt->mutex` around `get` and the call. `fork_session_pauseresume` was named in the review but never uses `PipeHandle::get`. Not built or run.
- [x] **`start` with only 4 tokens crashed FreeSWITCH.** The usage check was `argc < 4`, but `argv[4]` (sample rate) is read unconditionally, so `uuid_audio_fork <uuid> start <url> <mix>` ran `strcmp(NULL, ...)`. Now `argc < 5`. Not run against a live FreeSWITCH.
- [x] **`start` dereferenced a NULL read codec** (`mod_audio_fork.c`). The codec was fetched before `switch_channel_pre_answer` and used unchecked; it is now fetched after it and checked. `fork_data_init` also ignored the status of `switch_core_session_get_read_impl` and used an uninitialised `read_impl` if it failed; it now returns an error (the `private_t` is zeroed first so cleanup is safe). Not run against a live FreeSWITCH.
- [x] **C++ exceptions could terminate the process.** `AudioPipe::lws_callback` is now a wrapper that catches everything from `lws_callback_impl` and closes the connection; `fork_session_init`, `parse_ws_uri` (std::regex, std::string) and `AudioPipe::enqueueText` catch and return an error. `fork_frame` and `dub_speech_frame` do not allocate and were left alone.

## Medium

- [x] **No upper limit on the sample rates** (`mod_audio_fork.c` start parsing). The audio buffer size is computed in `int` (`320 * rate / 8000 * channels * 1000 / 20 * secs`) and overflowed above ~6.7M (the resulting `new[]` failure is caught now, but rates of 1M to 6M still allocated tens of MB per call); `bidirectional_audio_sample_rate` was unchecked too. Both are now limited to `MAX_SAMPLE_RATE` (64000, the documented maximum) and the bidirectional rate must not be negative. Not run against a live FreeSWITCH.
- [ ] **One `lws_cancel_service` and one `pendingWrites` entry per 20 ms frame per call** (`AudioPipe::unlockAudioBuffer` -> `addPendingWrite`). About 50 wakeups per second per call, all on the single lws thread. A per-pipe "write already pending" flag would remove nearly all of them.
- [ ] **First text fragment allocation is not bounded** (`audio_pipe.cpp` ~186). `malloc(len + lws_remaining_packet_payload)` uses the size the server announces; `MAX_RECV_BUF_SIZE` is only checked when the buffer grows later. After the limit is hit, the remaining fragments start a new buffer (`realloc(nullptr, ...)`) and the tail of the oversized message is delivered as a message of its own.
- [ ] **`findAndRemovePendingConnect` removes every entry with `m_wsi == nullptr`**, which includes pipes still `IDLE` and not yet connected. List order protects them in the normal case; a callback for a wsi that is not in the list would drop them, and they would never connect or report a failure. Remove only entries that are `FAILED`/`DISCONNECTED`.
- [ ] **`do_graceful_shutdown` / `addPendingWrite` / `addPendingDisconnect` dereference `m_vhd->context`**, which is null until the lws thread has started the connect (a graceful shutdown right after `start` crashes). Graceful shutdown is not used here, so low priority; use the static `context` and skip when null.
- [ ] **Graceful shutdown blocks closing.** After `graceful-shutdown` the WRITEABLE handler returns early on `isGracefulShutdown()` and never reaches the `DISCONNECTING` branch, so a later `stop` cannot close our side. Also logged at ERROR for a normal event.
- [x] **`close()` vs `LWS_CALLBACK_CLIENT_ESTABLISHED` race** (`audio_pipe.cpp` ~119, from `/code-review`). Checked, low: a second disconnect request for a connection that is already closing is harmless. The only loss is a final `stop` text that races with the connection being established.
- [x] **Short `lws_write` on a text frame closes the connection** (`audio_pipe.cpp` ~262, from `/code-review`). Checked, not an issue: lws buffers short writes and returns `n`; it returns < 0 only on a real error.
- [x] **`deinitialize` does not drain the static `pendingConnects` / `pendingDisconnects` / `pendingWrites`** (`audio_pipe.cpp` ~524, from `/code-review`). Checked, low: the static `Ptr`s are destroyed by the module's static destructors, which run before the library is unmapped; at worst they are a leak. Related to the open `fork_cleanup` item above.
- [x] **`dub_speech_frame` skips the whole frame when `clearPlayout` is set** (`lws_glue.cpp`, from `/code-review`). Checked, not an issue: after a clear there is nothing to mix, so passing the frame through untouched is correct. A try-lock miss only delays playout by one frame, because the buffered audio is kept.
- [x] **`fork_session_cleanup` returns early without `destroy_tech_pvt` when the bug private is missing** (from `/code-review`). Checked, not an issue: the early return is deliberate because whoever cleared the bug private under `tech_pvt->mutex` owns the teardown. A second caller (`stop` racing with a media-bug close) or the nested close triggered by `switch_core_media_bug_remove` finds no bug and must not destroy again. `start_capture` stores the bug right after `media_bug_add` succeeds (`set_private` cannot fail), and a failed `media_bug_add` goes through `fork_session_destroy`. The `tech_pvt->id` read before the null check is a separate minor item under Low.
- [x] **`connect_client` failure path** (`audio_pipe.cpp` ~399, from `/code-review`). Checked, low: `close()` in that window is a no-op by design (`m_closeRequested` is set), and `eventCallback` ignores events for a session whose bug is gone.
- [x] **Possible leak when `fork_data_init` fails** (`lws_glue.cpp` ~336, from `/code-review`). Checked, not an issue: `destroy_tech_pvt` releases the handles on every failure path and `start_capture` only continues after a successful init.

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

# Performance: Rust vs C mod_audio_fork

Measured with the load tier (`just test-load N`, [testing-plan.md](testing-plan.md#load-tier)) on 2026-10-12, during rebuild step 9, before the C module was removed.

**The comparison is not fair yet:** the test image built the C module without optimisation (`gcc`/`g++` with no `-O` flag, i.e. `-O0`; `Dockerfile` at `1a46b3e`, lines 53–57), the Rust module as a release build. Production builds the C module with FreeSWITCH's own (optimised) flags. Read the CPU difference as an upper bound, not as the real gap.

## Setup

- N loopback call pairs at 8 kHz L16, each with one fork: mono capture of leg A to a WebSocket server, and bidirectional playout of a stream the server sends in real time (20 ms frames), recorded on leg B.
- 60 s of steady state per run; one run per module and call count.
- FreeSWITCH 1.11.3 with a minimal module set, in Docker Desktop on macOS (arm64 VM, 10 CPUs, 7.6 GiB).
- CPU is the whole FreeSWITCH process (user + system, from `/proc/<pid>/stat`) over the 60 s, divided by N. Loopback channels, file playback and recording are the same in both runs, so the difference between the modules is the modules'.
- Playout delay: from the server's first frame to its first sample in leg B's recording.

## Results

| Calls | Module | CPU per call | Buffer overruns | Playout delay p50 / p90 / max |
|---|---|---|---|---|
| 20 | Rust | 1.26 % | 0 | 199 / 219 / 219 ms |
| 20 | C (`-O0`) | 1.71 % | 0 | 199 / 219 / 219 ms |
| 100 | Rust | 1.03 % | 0 | 199 / 199 / 219 ms |
| 100 | C (`-O0`) | 1.85 % | 0 | 198 / 199 / 218 ms |

A second 20-call Rust run, after the review fixes (`89a08c6`), gave 1.24 %.

Both modules show the same playout delay: they share the prebuffer formula (160 ms before the first release).

## Why Rust may use less CPU

Not profiled; these are the differences in the per-frame code that could account for part of the gap once both are optimised.

- **C needs a wake-up per frame.** For every 20 ms frame of every call, `unlockAudioBuffer` → `addPendingWrite` takes a global mutex, copies a `shared_ptr` and calls `lws_cancel_service`, a pipe write that wakes the single lws service thread, which then asks lws for a writeable callback per connection. In Rust the media thread pushes into a lock-free ring and calls `Notify::notify_one`, which only wakes a tokio worker that is parked.
- **C zeroes 16 KB per played frame.** `dub_speech_frame` clears `int16_t data[SWITCH_RECOMMENDED_BUFFER_SIZE]` (8192 samples) to copy 160 into it; Rust mixes straight from the playout queue.
- **C copies more per received message.** `processIncomingBinary` goes through a `std::vector`, the prebuffer, another vector and the playout buffer.

At `-O0` the C++ side additionally pays for `boost::circular_buffer`'s iterator code (`insert`, `erase`, `copy_n`) not being inlined, per sample. That alone may explain much of the measured difference.

## Not measured

Memory; stereo, resampling, `wss://` and capture without playout; production hardware (x86, fs-image's build); variance (single runs).

## To get a fair comparison

1. In a scratch worktree at `1a46b3e` (the last commit with the C build), build the C module with `-O2 -g`.
2. Run `just test-load 100` several times per module (e.g. 5 each) and report mean and spread.
3. Optionally profile both with `perf record -g -p <freeswitch pid>` (the container needs `--cap-add SYS_ADMIN`, or a lower `perf_event_paranoid`).

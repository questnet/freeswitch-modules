# Performance: Rust vs C mod_audio_fork

Measured with the load tier (`just test-load N`, [testing-plan.md](testing-plan.md#load-tier)) on 2026-10-12.

**Result:** with both modules optimised, FreeSWITCH used 1.00 % CPU per call with the Rust module and 1.29 % with the C module (mean of 5 runs of 100 calls each), with no buffer overruns and the same playout delay. The Rust module's runs also varied less.

## Setup

- N loopback call pairs at 8 kHz L16, each with one fork: mono capture of leg A to a WebSocket server, and bidirectional playout of a stream the server sends in real time (20 ms frames), recorded on leg B.
- 60 s of steady state per run.
- FreeSWITCH 1.11.3 with a minimal module set, in Docker Desktop on macOS (arm64 VM, 10 CPUs, 7.6 GiB).
- CPU is the whole FreeSWITCH process (user + system, from `/proc/<pid>/stat`) over the 60 s, divided by N. Loopback channels, file playback and recording are the same in both runs, so the difference between the modules is the modules'.
- Playout delay: from the server's first frame to its first sample in leg B's recording.

## Results

### Both optimised (fair)

C module built with FreeSWITCH's own module flags (`-g -O2 -fPIC -ffast-math -fvisibility=hidden` and its visibility defines, without `-Werror`), from the last commit with the C build (`1a46b3e`), in a scratch worktree; it passed every real-FS scenario in that build. Rust module from `3e7d15a` (release build). Same FreeSWITCH image. 5 runs of 100 calls per module, alternating C and Rust.

| Run | C, CPU per call | Rust, CPU per call |
|---|---|---|
| 1 | 1.48 % | 1.01 % |
| 2 | 1.26 % | 1.00 % |
| 3 | 1.22 % | 0.99 % |
| 4 | 1.16 % | 0.97 % |
| 5 | 1.33 % | 1.03 % |
| **mean ± sd** | **1.29 ± 0.12 %** | **1.00 ± 0.02 %** |

Every run: 0 buffer overruns, 100 of 100 calls played, playout delay p50 and p90 199 ms, max 199–239 ms, for both modules.

The difference, 0.29 percentage points per call, is FreeSWITCH's whole CPU per call; the modules' own share of it was not measured (that would need a baseline run with the same calls and no fork).

### Rust after the event queue

Rust module at `cd8927a`: events of all forks go through one bounded queue fired by one task, and the media thread takes the fork's state lock for every frame (review 2, finding 1.5). Same image and setup, 3 runs of 100 calls.

| Run | Rust, CPU per call |
|---|---|
| 1 | 1.09 % |
| 2 | 1.04 % |
| 3 | 1.00 % |
| **mean ± sd** | **1.04 ± 0.05 %** |

Every run: 0 buffer overruns, 100 of 100 calls played, playout delay p50 and p90 199 ms, max 219 ms. Within 0.04 points of the earlier mean, and every run below the C module's lowest; no regression.

### First measurement, C unoptimised (superseded)

During rebuild step 9 the test image built the C module without optimisation (`gcc`/`g++` with no `-O` flag, i.e. `-O0`; `Dockerfile` at `1a46b3e`, lines 53–57), so these overstate the gap. One run each.

| Calls | Module | CPU per call | Buffer overruns | Playout delay p50 / p90 / max |
|---|---|---|---|---|
| 20 | Rust | 1.26 % | 0 | 199 / 219 / 219 ms |
| 20 | C (`-O0`) | 1.71 % | 0 | 199 / 219 / 219 ms |
| 100 | Rust | 1.03 % | 0 | 199 / 199 / 219 ms |
| 100 | C (`-O0`) | 1.85 % | 0 | 198 / 199 / 218 ms |

A second 20-call Rust run, after the review fixes (`89a08c6`), gave 1.24 %.

Both modules show the same playout delay: they share the prebuffer formula (160 ms before the first release).

## Why Rust may use less CPU

Not profiled; these are the differences in the per-frame code that could account for the gap.

- **C needs a wake-up per frame.** For every 20 ms frame of every call, `unlockAudioBuffer` → `addPendingWrite` takes a global mutex, copies a `shared_ptr` and calls `lws_cancel_service`, a pipe write that wakes the single lws service thread, which then asks lws for a writeable callback per connection. In Rust the media thread pushes into a lock-free ring and calls `Notify::notify_one`, which only wakes a tokio worker that is parked.
- **C zeroes 16 KB per played frame.** `dub_speech_frame` clears `int16_t data[SWITCH_RECOMMENDED_BUFFER_SIZE]` (8192 samples) to copy 160 into it; Rust mixes straight from the playout queue.
- **C copies more per received message.** `processIncomingBinary` goes through a `std::vector`, the prebuffer, another vector and the playout buffer.

In the unoptimised build the C++ side additionally paid for `boost::circular_buffer`'s iterator code (`insert`, `erase`, `copy_n`) not being inlined, which explains why that measurement showed a larger gap.

## Not measured

The modules' own CPU share (a baseline without forks); memory; stereo, resampling, `wss://` and capture without playout; production hardware (x86, fs-image's build); a profile.

## To measure again

`just test-load N` for the Rust module. The C module needs the build from `1a46b3e` with the flags above. A profile of both would show where the difference comes from: `perf record -g -p <freeswitch pid>` in the container (it needs `--cap-add SYS_ADMIN`, or a lower `perf_event_paranoid`).

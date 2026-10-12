//! Load tier (docs/testing-plan.md#load-tier): N calls forking with bidirectional audio for 60 s,
//! reporting FreeSWITCH's CPU per call, buffer overruns and the playout delay. Run by `just test-load N`.

use std::env;
use std::time::Duration;

use fs_tests::FreeSwitch;
use test_server::Streamer;

const SECONDS: u64 = 60;

fn main() {
    let calls: usize = env::args()
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(20);
    let report = fs_tests::with_freeswitch(|fs| Box::pin(load(fs, calls)));
    println!("{report}");
}

async fn load(fs: &FreeSwitch, n: usize) -> String {
    let streamer = Streamer::start(8000).await;
    let mut calls = Vec::new();
    for _ in 0..n {
        let call = fs.call(8000).await;
        call.play_counter().await;
        call.play_silence_to_a().await;
        let (recording, recording_started) = call.record_b_timed().await;
        let url = streamer.url(&format!("/{}", call.a));
        call.fork(&format!("start {url} mono 8k load {{}} true true 8000"))
            .await;
        calls.push((call, recording, recording_started));
    }
    let cpu_before = fs.cpu_time();
    tokio::time::sleep(Duration::from_secs(SECONDS)).await;
    let cpu = fs.cpu_time() - cpu_before;

    let mut delays = Vec::new();
    for (call, recording, recording_started) in &calls {
        call.fork("stop load").await;
        let played = first_counter(&call.recorded_b(recording).await);
        let started = streamer
            .started(&format!("/{}", call.a))
            .expect("never streamed");
        if let Some(index) = played {
            let at = *recording_started + Duration::from_secs_f64(index as f64 / 8000.0);
            delays.push(at.saturating_duration_since(started).as_millis());
        }
    }
    for (call, _, _) in calls {
        call.hangup().await;
    }
    delays.sort();
    let percentile = |p: usize| {
        delays
            .get(delays.len().saturating_sub(1) * p / 100)
            .copied()
            .unwrap_or(0)
    };
    let cpu_per_call = cpu.as_secs_f64() / SECONDS as f64 * 100.0 / n as f64;
    format!(
        "{:?}: {n} calls, {cpu_per_call:.2} % CPU per call, {} buffer overruns, playout delay ms p50 {} p90 {} max {} ({} of {n} calls played)",
        fs.variant,
        fs.overruns(),
        percentile(50),
        percentile(90),
        delays.last().copied().unwrap_or(0),
        delays.len(),
    )
}

/// Where in a recording the streamed counter would have started: the first run of 16 consecutive
/// values, moved back by the value it starts at (the first frame played can be missing from it).
/// The run can begin on the silent 0 just before the counter's 1.
fn first_counter(samples: &[i16]) -> Option<usize> {
    let consecutive = |w: &[i16]| w.windows(2).all(|p| p[1] == p[0].wrapping_add(1));
    let index = samples.windows(16).position(consecutive)?;
    let value = samples[index] as u16 as i64;
    usize::try_from(index as i64 - (value - 1)).ok()
}

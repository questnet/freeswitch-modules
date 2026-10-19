//! Real-FreeSWITCH scenarios (docs/testing-plan.md#scenarios). Run in the fs-test image by `just test-fs`.

use std::time::Duration;

use fs_tests::audio::{assert_counter, channel, frequency, samples};
use fs_tests::{FreeSwitch, Needs, Scenario};
use test_server::{Frame, Server, Sink};
use tokio::time;

/// `(name, needs, scenario)` entries, named after their functions.
macro_rules! scenarios {
    ($($needs:ident $name:ident),* $(,)?) => {
        &[$((stringify!($name), Needs::$needs, |fs| Box::pin($name(fs)))),*]
    };
}

fn main() {
    let scenarios: &[(&str, Needs, Scenario)] = scenarios![
        Any load_lifecycle,
        Any start_rejections,
        Any capture,
        Any two_forks,
        Any events,
        Any inbound,
        Any receive_limit,
        Any bidi_playout,
        Any bidi_playout_resampled,
        Any kill_audio,
        Any playback_mode,
        Any hangup_races,
        Any unload_with_live_forks,
        Any unload_during_hangups,
        Hooks panic_containment,
    ];
    fs_tests::run(scenarios);
}

/// Unload and reload leave a working module that answers a bare command with its usage.
async fn load_lifecycle(fs: &FreeSwitch) {
    fs.api_ok("unload mod_audio_fork").await;
    assert_eq!(fs.api("module_exists mod_audio_fork").await.trim(), "false");
    fs.api_ok("load mod_audio_fork").await;
    fs.api_ok("reload mod_audio_fork").await;
    for cmd in [
        "uuid_audio_fork",
        "uuid_audio_fork 00000000-0000-0000-0000-000000000000",
    ] {
        let reply = fs.api(cmd).await;
        assert!(reply.starts_with("-USAGE: "), "{cmd}: {reply}");
    }
}

/// Bad start arguments are answered with the usage or an error and attach no bug.
async fn start_rejections(fs: &FreeSwitch) {
    let call = fs.call(8000).await;
    let url = "ws://127.0.0.1:1/";
    let usage = [
        format!("start {url} mono"),
        format!("start {url} quad 8k rej"),
        format!("start {url} mono 12000 rej"),
        format!("start {url} mono 72000 rej"),
        format!("start {url} mono 8k rej {{}} true true 72000 x"),
    ];
    for args in &usage {
        let reply = fs.api(&format!("uuid_audio_fork {} {args}", call.a)).await;
        assert!(reply.starts_with("-USAGE: "), "{args}: {reply}");
    }
    let reply = fs
        .api(&format!(
            "uuid_audio_fork 00000000-0000-0000-0000-000000000000 start {url} mono 8k rej"
        ))
        .await;
    assert!(reply.starts_with("-ERR"), "unknown uuid: {reply}");
    let reply = fs
        .api(&format!("uuid_audio_fork {} pause rej", call.a))
        .await;
    assert!(reply.starts_with("-ERR"), "unsupported subcommand: {reply}");
    let big = format!("{{\"m\":\"{}\"}}", "x".repeat(1024 * 1024));
    let reply = fs
        .api(&format!(
            "uuid_audio_fork {} start {url} mono 8k rej {big}",
            call.a
        ))
        .await;
    assert!(reply.starts_with("-ERR"), "metadata over 1 MB: {reply}");
    let bugs = call.bugs().await;
    assert!(!bugs.contains("rej"), "bugs left: {bugs}");

    call.fork(&format!("start {url} mono 8k dup")).await;
    let reply = fs
        .api(&format!(
            "uuid_audio_fork {} start {url} mono 8k dup",
            call.a
        ))
        .await;
    assert!(reply.starts_with("-ERR"), "duplicate bugname: {reply}");
    call.fork("stop dup").await;
    let reply = fs
        .api(&format!("uuid_audio_fork {} stop dup", call.a))
        .await;
    assert!(reply.starts_with("-ERR"), "stop after stop: {reply}");
    call.hangup().await;
}

/// Every mix at both codec and target rates: the counter arrives gapless where nothing is
/// resampled, the tone keeps its frequency where it is.
async fn capture(fs: &FreeSwitch) {
    for codec in [8000, 16000] {
        for rate in [8000, 16000] {
            for mix in ["mono", "mixed", "stereo"] {
                let what = format!("{mix} {codec} Hz -> {rate} Hz");
                println!("  {what}");
                let mut server = Server::start().await;
                let call = fs.call(codec).await;
                call.fork(&format!("start {} {mix} {rate} cap", server.url("/")))
                    .await;
                let mut conn = server.accept().await;
                let channels = if mix == "stereo" { 2 } else { 1 };
                if mix != "mono" {
                    call.play_silence_to_a().await;
                }
                if codec == rate {
                    call.play_counter().await;
                } else {
                    call.play_tone(1000).await;
                }
                let audio = samples(&conn.audio(rate as usize * channels * 2).await);
                let read = channel(&audio, channels, 0);
                if codec == rate {
                    let n = assert_counter(&read);
                    assert!(n > rate as usize / 4, "{what}: only {n} counter samples");
                } else {
                    let hz = frequency(&read, rate);
                    assert!((950.0..1050.0).contains(&hz), "{what}: tone at {hz} Hz");
                }
                call.fork("stop cap").await;
                call.hangup().await;
            }
        }
    }
}

/// Two bugnames on one call stream independently; stopping one leaves the other.
async fn two_forks(fs: &FreeSwitch) {
    let mut server = Server::start().await;
    let call = fs.call(8000).await;
    call.fork(&format!("start {} mono 8k one", server.url("/one")))
        .await;
    let mut one = server.accept().await;
    call.fork(&format!("start {} mono 8k two", server.url("/two")))
        .await;
    let mut two = server.accept().await;
    call.play_counter().await;
    assert_counter(&samples(&one.audio(8000).await));
    assert_counter(&samples(&two.audio(8000).await));
    call.fork("stop one").await;
    loop {
        match one.next().await {
            Some(Frame::Binary(_)) => {}
            Some(Frame::Close) | None => break,
            Some(other) => panic!("after stop: {other:?}"),
        }
    }
    let after = samples(&two.audio(8000).await);
    assert!(
        after.iter().all(|&s| s != 0),
        "second fork stalled after the first stopped"
    );
    call.fork("stop two").await;
    call.hangup().await;
}

/// connect, disconnect and connect_failed carry the channel data and the bugname.
async fn events(fs: &FreeSwitch) {
    let mut server = Server::start().await;
    let call = fs.call(8000).await;
    call.fork(&format!("start {} mono 8k ev", server.url("/")))
        .await;
    let conn = server.accept().await;
    let connect = fs.event(&call.a, "ev", "mod_audio_fork::connect").await;
    assert!(
        connect.channel_name.is_some(),
        "no channel data: {connect:?}"
    );
    assert_eq!(connect.body, None);
    conn.drop_tcp();
    let disconnect = fs.event(&call.a, "ev", "mod_audio_fork::disconnect").await;
    assert_eq!(disconnect.body, None);

    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    call.fork(&format!("start ws://127.0.0.1:{port}/ mono 8k failed"))
        .await;
    let failed = fs
        .event(&call.a, "failed", "mod_audio_fork::connect_failed")
        .await;
    let body: serde_json::Value = serde_json::from_str(failed.body.as_deref().unwrap()).unwrap();
    assert!(body["reason"].is_string(), "{body}");
    call.fork("stop ev").await;
    call.fork("stop failed").await;
    call.hangup().await;
}

/// Server messages become events with the right bodies; unknown types fire nothing.
async fn inbound(fs: &FreeSwitch) {
    let mut server = Server::start().await;
    let call = fs.call(8000).await;
    call.fork(&format!("start {} mono 8k in", server.url("/")))
        .await;
    let conn = server.accept().await;
    fs.event(&call.a, "in", "mod_audio_fork::connect").await;
    conn.send_text(r#"{"type":"transfer","data":{"to":"x"}}"#);
    conn.send_text(r#"{"type":"json","z":1,"a":{"b":[1,2]}}"#);
    conn.send_text(r#"{"type":"error","data":{"code":"x"}}"#);
    conn.send_text(r#"{"type":"disconnect"}"#);
    conn.send_text(r#"{"type":"killAudio"}"#);
    fs.event(&call.a, "in", "mod_audio_fork::kill_audio").await;
    let fired: Vec<(String, Option<String>)> = fs
        .events_of(&call.a)
        .into_iter()
        .map(|e| (e.subclass, e.body))
        .collect();
    let expected = [
        ("mod_audio_fork::connect", None),
        (
            "mod_audio_fork::json",
            Some(r#"{"type":"json","z":1,"a":{"b":[1,2]}}"#),
        ),
        ("mod_audio_fork::error", Some(r#"{"code":"x"}"#)),
        ("mod_audio_fork::disconnect", None),
        ("mod_audio_fork::kill_audio", None),
    ]
    .map(|(s, b)| (s.to_string(), b.map(str::to_string)));
    assert_eq!(fired, expected);
    call.fork("stop in").await;
    call.hangup().await;
}

/// A text message over 1 MB closes the connection.
async fn receive_limit(fs: &FreeSwitch) {
    let mut server = Server::start().await;
    let call = fs.call(8000).await;
    call.fork(&format!("start {} mono 8k big", server.url("/")))
        .await;
    let conn = server.accept().await;
    fs.event(&call.a, "big", "mod_audio_fork::connect").await;
    conn.send_text(format!(
        r#"{{"type":"json","x":"{}"}}"#,
        "x".repeat(1024 * 1024)
    ));
    fs.event(&call.a, "big", "mod_audio_fork::disconnect").await;
    call.fork("stop big").await;
    call.hangup().await;
}

/// Server audio is played to the caller unchanged (no resampling at matching rates).
async fn bidi_playout(fs: &FreeSwitch) {
    for rate in [8000, 16000] {
        let mut server = Server::start().await;
        let call = fs.call(rate).await;
        call.play_silence_to_a().await;
        let recording = call.record_b().await;
        call.fork(&format!(
            "start {} mono {rate} pb {{}} true true {rate}",
            server.url("/")
        ))
        .await;
        let conn = server.accept().await;
        send_counter(&conn, rate, 2);
        time::sleep(Duration::from_millis(2600)).await;
        let played = call.recorded_b(&recording).await;
        let n = assert_counter(&played);
        assert!(
            n >= rate as usize * 3 / 2,
            "{rate} Hz: only {n} samples played"
        );
        call.fork("stop pb").await;
        call.hangup().await;
    }
}

/// Streams `seconds` of the running counter (from 1) in frames that split samples.
fn send_counter(conn: &test_server::Conn, rate: u32, seconds: u32) {
    let bytes: Vec<u8> = (1..=rate * seconds)
        .flat_map(|i| (i as i16).to_le_bytes())
        .collect();
    for chunk in bytes.chunks(333) {
        conn.send_binary(chunk.to_vec());
    }
}

/// A 16 kHz stream played into an 8 kHz call keeps its frequency.
async fn bidi_playout_resampled(fs: &FreeSwitch) {
    let mut server = Server::start().await;
    let call = fs.call(8000).await;
    call.play_silence_to_a().await;
    let recording = call.record_b().await;
    call.fork(&format!(
        "start {} mono 8k pr {{}} true true 16000",
        server.url("/")
    ))
    .await;
    let conn = server.accept().await;
    let tone: Vec<u8> = (0..32000)
        .map(|i| (8000.0 * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / 16000.0).sin()) as i16)
        .flat_map(|s| s.to_le_bytes())
        .collect();
    for chunk in tone.chunks(640) {
        conn.send_binary(chunk.to_vec());
    }
    time::sleep(Duration::from_millis(2600)).await;
    let played = call.recorded_b(&recording).await;
    let hz = frequency(&played, 8000);
    assert!((950.0..1050.0).contains(&hz), "played at {hz} Hz");
    call.fork("stop pr").await;
    call.hangup().await;
}

/// killAudio stops the playout and interrupts what leg A plays.
async fn kill_audio(fs: &FreeSwitch) {
    let mut server = Server::start().await;
    let call = fs.call(8000).await;
    call.play_silence_to_a().await;
    let recording = call.record_b().await;
    call.fork(&format!(
        "start {} mono 8k ka {{}} true true 8000",
        server.url("/")
    ))
    .await;
    let conn = server.accept().await;
    send_counter(&conn, 8000, 5);
    time::sleep(Duration::from_millis(1000)).await;
    let interrupted = |fs: &FreeSwitch| {
        fs.events_all(&call.a).iter().any(|e| {
            e.subclass == "CHANNEL_EXECUTE_COMPLETE" && e.application.as_deref() == Some("playback")
        })
    };
    assert!(!interrupted(fs), "the endless silence ended by itself");
    conn.send_text(r#"{"type":"killAudio"}"#);
    fs.event(&call.a, "ka", "mod_audio_fork::kill_audio").await;
    // the endless silence leg A plays can only complete when it is interrupted
    fs.event(&call.a, "", "CHANNEL_EXECUTE_COMPLETE").await;
    assert!(interrupted(fs), "playback not interrupted");
    time::sleep(Duration::from_millis(1000)).await;
    let played = assert_counter(&call.recorded_b(&recording).await);
    assert!(
        played < 8000 * 2,
        "{played} samples played, killAudio came after about 8000"
    );
    call.fork("stop ka").await;
    call.hangup().await;
}

/// `playback`: server audio is played, the caller's is not sent.
async fn playback_mode(fs: &FreeSwitch) {
    let mut server = Server::start().await;
    let call = fs.call(8000).await;
    call.play_silence_to_a().await;
    call.play_counter().await;
    let recording = call.record_b().await;
    call.fork(&format!(
        "start {} mono 8k po {{}} playback true 8000",
        server.url("/")
    ))
    .await;
    let mut conn = server.accept().await;
    send_counter(&conn, 8000, 2);
    assert_eq!(conn.next().await, Some(Frame::Text("{}".to_string())));
    let sent = time::timeout(Duration::from_millis(2500), conn.next()).await;
    assert!(sent.is_err(), "the server got {sent:?}");
    assert!(assert_counter(&call.recorded_b(&recording).await) >= 8000 * 3 / 2);
    call.fork("stop po").await;
    call.hangup().await;
}

/// Hangup during the connect, during streaming, and a stop racing the hangup, 200 times each:
/// FreeSWITCH stays up and no socket is left open.
async fn hangup_races(fs: &FreeSwitch) {
    let slow = Sink::start(Duration::from_millis(50)).await;
    let sink = Sink::start(Duration::ZERO).await;
    for i in 0..200 {
        let call = fs.call(8000).await;
        call.fork(&format!("start {} mono 8k race", slow.url("/")))
            .await;
        call.hangup().await;

        let call = fs.call(8000).await;
        call.play_counter().await;
        call.fork(&format!("start {} mono 8k race", sink.url("/")))
            .await;
        if i % 2 == 0 {
            time::sleep(Duration::from_millis(30)).await;
        }
        call.hangup().await;

        let call = fs.call(8000).await;
        call.fork(&format!("start {} mono 8k race", sink.url("/")))
            .await;
        fs.api(&format!("bgapi uuid_kill {}", call.a)).await;
        // either the stop or the hangup gets there first
        let reply = fs
            .api(&format!("uuid_audio_fork {} stop race", call.a))
            .await;
        assert!(
            reply.starts_with("+OK") || reply.starts_with("-ERR"),
            "{reply}"
        );
    }
    assert!(fs.api("status").await.contains("UP"));
    all_closed(&slow).await;
    all_closed(&sink).await;
    assert!(sink.accepted() > 0);
}

/// Waits until the server has no connection left open.
async fn all_closed(sink: &Sink) {
    for _ in 0..150 {
        if sink.open() == 0 {
            return;
        }
        time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "{} of {} connections left open",
        sink.open(),
        sink.accepted()
    );
}

/// Unloading with live forks removes their bugs and closes their sockets; the calls survive.
async fn unload_with_live_forks(fs: &FreeSwitch) {
    let sink = Sink::start(Duration::ZERO).await;
    let one = fs.call(8000).await;
    let two = fs.call(8000).await;
    for call in [&one, &two] {
        call.play_counter().await;
        call.fork(&format!("start {} mono 8k live", sink.url("/")))
            .await;
        fs.event(&call.a, "live", "mod_audio_fork::connect").await;
    }
    assert_eq!(sink.open(), 2);
    fs.api_ok("unload mod_audio_fork").await;
    all_closed(&sink).await;
    for call in [&one, &two] {
        assert!(!call.bugs().await.contains("live"), "bug left after unload");
    }
    fs.api_ok("load mod_audio_fork").await;
    one.fork(&format!("start {} mono 8k again", sink.url("/")))
        .await;
    fs.event(&one.a, "again", "mod_audio_fork::connect").await;
    one.hangup().await;
    two.hangup().await;
    all_closed(&sink).await;
}

/// A panic in the media callback or the socket task ends that fork with `error`; another call
/// keeps streaming without a gap.
async fn panic_containment(fs: &FreeSwitch) {
    fs.allow_crit("panic in media bug callback: injected in the media callback");
    fs.allow_crit("socket task panicked: injected in the socket task");
    let mut server = Server::start().await;
    let one = fs.call(8000).await;
    let two = fs.call(8000).await;
    one.play_counter().await;
    two.play_counter().await;
    one.fork(&format!("start {} mono 8k p1", server.url("/one")))
        .await;
    let mut one_conn = server.accept().await;
    two.fork(&format!("start {} mono 8k p2", server.url("/two")))
        .await;
    let mut two_conn = server.accept().await;
    two_conn.audio(4000).await;

    one.fork("test_panic_media p1").await;
    let error = fs.event(&one.a, "p1", "mod_audio_fork::error").await;
    assert!(
        error
            .body
            .unwrap()
            .contains("injected in the media callback")
    );
    loop {
        match one_conn.next().await {
            Some(Frame::Binary(_)) => {}
            Some(Frame::Close) | None => break,
            other => panic!("{other:?}"),
        }
    }
    assert!(
        !one.bugs().await.contains("p1"),
        "bug left after the media panic"
    );
    assert_counter(&samples(&two_conn.audio(16000).await));

    two.fork("test_panic_task p2").await;
    let error = fs.event(&two.a, "p2", "mod_audio_fork::error").await;
    assert!(error.body.unwrap().contains("injected in the socket task"));
    for _ in 0..50 {
        if !two.bugs().await.contains("p2") {
            break;
        }
        time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        !two.bugs().await.contains("p2"),
        "bug left after the task panic"
    );
    assert!(fs.api("status").await.contains("UP"));
    one.hangup().await;
    two.hangup().await;
}

/// Unloading while calls are hanging up: their bugs are closed before the module goes, so
/// FreeSWITCH neither crashes nor logs a CRIT, and every socket is closed.
async fn unload_during_hangups(fs: &FreeSwitch) {
    let sink = Sink::start(Duration::ZERO).await;
    let mut calls = Vec::new();
    for _ in 0..20 {
        let call = fs.call(8000).await;
        call.play_counter().await;
        call.fork(&format!("start {} mono 8k going", sink.url("/")))
            .await;
        fs.event(&call.a, "going", "mod_audio_fork::connect").await;
        calls.push(call);
    }
    for call in &calls {
        fs.api(&format!("bgapi uuid_kill {}", call.a)).await;
    }
    fs.api_ok("unload mod_audio_fork").await;
    all_closed(&sink).await;
    assert!(fs.api("status").await.contains("UP"));
    fs.api_ok("load mod_audio_fork").await;
    let call = fs.call(8000).await;
    call.fork(&format!("start {} mono 8k after", sink.url("/")))
        .await;
    fs.event(&call.a, "after", "mod_audio_fork::connect").await;
    call.hangup().await;
    all_closed(&sink).await;
}

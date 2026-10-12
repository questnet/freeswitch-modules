//! Real-FreeSWITCH scenarios (docs/testing-plan.md#scenarios). Run in the fs-test image by `just test-fs`.

use fs_tests::{
    Exception, FreeSwitch, Scenario,
    audio::{assert_counter, channel, frequency, samples},
};
use test_server::{Frame, Server};

fn main() {
    let scenarios: &[(&str, Scenario)] = &[
        ("load_lifecycle", |fs| Box::pin(load_lifecycle(fs))),
        ("start_rejections", |fs| Box::pin(start_rejections(fs))),
        ("capture", |fs| Box::pin(capture(fs))),
        ("two_forks", |fs| Box::pin(two_forks(fs))),
        ("events", |fs| Box::pin(events(fs))),
        ("inbound", |fs| Box::pin(inbound(fs))),
        ("receive_limit", |fs| Box::pin(receive_limit(fs))),
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
    for (i, args) in usage.iter().enumerate() {
        let reply = fs.api(&format!("uuid_audio_fork {} {args}", call.a)).await;
        // the C module answers a too short start with the usage, everything else silently
        if i > 0 && fs.shows(Exception::SilentRejection) {
            assert_eq!(reply, "-ERR no reply\n", "{args}");
        } else {
            assert!(reply.starts_with("-USAGE: "), "{args}: {reply}");
        }
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
    if fs.shows(Exception::NoReceiveLimit) {
        fs.event(&call.a, "big", "mod_audio_fork::json").await;
    } else {
        fs.event(&call.a, "big", "mod_audio_fork::disconnect").await;
    }
    call.fork("stop big").await;
    call.hangup().await;
}

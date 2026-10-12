//! Simulation tier (docs/testing-plan.md#simulation-tier): the core against a scripted server,
//! without FreeSWITCH. Frames are pushed by count; races are made deterministic by holding the
//! server's handshake until the test releases it.

mod common;

use std::time::Duration;

use audio_fork::fork::{Event, MAX_TEXT_LEN, MediaSide, TextError, Timeouts};
use common::{FRAME, start, start_with};
use test_server::{Frame, Server};

#[tokio::test(flavor = "multi_thread")]
async fn metadata_then_queued_texts_then_audio() {
    let mut server = Server::start().await;
    let (mut media, handle, host) =
        start(&server.url("/fork"), Some("{\"m\":1}"), Timeouts::default());
    handle.send_text("t1".to_string()).unwrap();
    handle.send_text("t2".to_string()).unwrap();
    // nobody to send to yet: dropped, not buffered
    media.on_read(&[1; 160]);
    let mut conn = server.accept().await;
    assert_eq!(conn.protocol.as_deref(), Some("audio.drachtio.org"));
    assert_eq!(conn.path, "/fork");
    host.wait_for(|e| *e == Event::Connect).await;
    media.on_read(&FRAME);
    assert_eq!(
        conn.next().await,
        Some(Frame::Text("{\"m\":1}".to_string()))
    );
    assert_eq!(conn.next().await, Some(Frame::Text("t1".to_string())));
    assert_eq!(conn.next().await, Some(Frame::Text("t2".to_string())));
    let audio = conn.audio(320).await;
    assert_eq!(audio.len(), 320);
    assert!(
        audio
            .chunks(2)
            .all(|s| i16::from_le_bytes([s[0], s[1]]) == 7)
    );
    media.close();
}

#[tokio::test(flavor = "multi_thread")]
async fn text_limits() {
    let mut server = Server::start().await;
    let (media, handle, _) = start(&server.url("/"), None, Timeouts::default());
    let mb = "x".repeat(MAX_TEXT_LEN);
    assert_eq!(handle.send_text(format!("{mb}x")), Err(TextError::TooLong));
    for _ in 0..4 {
        handle.send_text(mb.clone()).unwrap();
    }
    assert_eq!(handle.send_text("x".to_string()), Err(TextError::QueueFull));
    let _held = server.pending().await;
    media.close();

    let (media, handle, _) = start(&server.url("/"), None, Timeouts::default());
    for _ in 0..1024 {
        handle.send_text("x".to_string()).unwrap();
    }
    assert_eq!(handle.send_text("x".to_string()), Err(TextError::QueueFull));
    media.close();
}

#[tokio::test(flavor = "multi_thread")]
async fn refused_connect_fires_connect_failed_once() {
    // a port nobody listens on
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let (mut media, handle, host) = start(
        &format!("ws://127.0.0.1:{port}/"),
        None,
        Timeouts::default(),
    );
    let Event::ConnectFailed { reason } = host
        .wait_for(|e| matches!(e, Event::ConnectFailed { .. }))
        .await
    else {
        unreachable!()
    };
    assert!(!reason.is_empty());
    for _ in 0..1000 {
        media.on_read(&FRAME);
    }
    assert_eq!(handle.send_text("x".to_string()), Err(TextError::NotOpen));
    assert_eq!(host.events().len(), 1);
    media.close();
}

#[tokio::test(flavor = "multi_thread")]
async fn handshake_timeout_fires_connect_failed() {
    let mut server = Server::start().await;
    let timeouts = Timeouts {
        connect: Duration::from_millis(200),
        ..Timeouts::default()
    };
    let (media, _handle, host) = start(&server.url("/"), None, timeouts);
    let _held = server.pending().await;
    let failed = host
        .wait_for(|e| matches!(e, Event::ConnectFailed { .. }))
        .await;
    assert_eq!(
        failed,
        Event::ConnectFailed {
            reason: "connect timed out".to_string()
        }
    );
    media.close();
}

#[tokio::test(flavor = "multi_thread")]
async fn stalled_server_overruns_once() {
    let mut server = Server::start().await;
    let (mut media, _handle, host) = start(&server.url("/"), None, Timeouts::default());
    let conn = server.accept().await;
    host.wait_for(|e| *e == Event::Connect).await;
    conn.stall();
    // until the socket buffers and then the 2 s ring are full; one hour of audio is far more than that
    for i in 0..180_000 {
        media.on_read(&FRAME);
        if host.events().contains(&Event::BufferOverrun) {
            break;
        }
        if i % 50 == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
    host.wait_for(|e| *e == Event::BufferOverrun).await;
    for _ in 0..1000 {
        media.on_read(&FRAME);
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let overruns = host
        .events()
        .iter()
        .filter(|e| **e == Event::BufferOverrun)
        .count();
    assert_eq!(overruns, 1);
    media.close();
}

#[tokio::test(flavor = "multi_thread")]
async fn dropped_connection_fires_disconnect() {
    let mut server = Server::start().await;
    let (mut media, handle, host) = start(&server.url("/"), None, Timeouts::default());
    let conn = server.accept().await;
    host.wait_for(|e| *e == Event::Connect).await;
    conn.drop_tcp();
    host.wait_for(|e| *e == Event::Disconnect(None)).await;
    media.on_read(&FRAME);
    assert_eq!(handle.send_text("x".to_string()), Err(TextError::NotOpen));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(host.events(), vec![Event::Connect, Event::Disconnect(None)]);
    media.close();
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_sends_the_final_text_before_closing() {
    let mut server = Server::start().await;
    let (mut media, handle, host) = start(&server.url("/"), None, Timeouts::default());
    let mut conn = server.accept().await;
    host.wait_for(|e| *e == Event::Connect).await;
    media.on_read(&FRAME);
    conn.audio(320).await;
    handle.stop(Some("{\"final\":true}".to_string()));
    assert_eq!(
        handle.send_text("late".to_string()),
        Err(TextError::NotOpen)
    );
    media.close();
    assert_eq!(conn.next_text().await, Some("{\"final\":true}".to_string()));
    assert_eq!(conn.next().await, Some(Frame::Close));
    assert_eq!(conn.next().await, None);
    assert_eq!(host.events(), vec![Event::Connect]);
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_while_connecting_sends_nothing() {
    let mut server = Server::start().await;
    let (mut media, handle, host) = start(&server.url("/"), Some("{}"), Timeouts::default());
    let pending = server.pending().await;
    handle.send_text("queued".to_string()).unwrap();
    media.on_read(&FRAME);
    handle.stop(Some("final".to_string()));
    media.close();
    // the connection attempt is dropped: the handshake fails, or the connection ends at once
    if let Some(mut conn) = pending.try_accept().await {
        assert_eq!(conn.next().await, None);
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(host.events(), vec![]);
}

#[tokio::test(flavor = "multi_thread")]
async fn hangup_during_connect_cancels_without_events() {
    let mut server = Server::start().await;
    let (media, _handle, host) = start(&server.url("/"), None, Timeouts::default());
    let _held = server.pending().await;
    media.close();
    // the connect timeout would fire a connect_failed if the task were still waiting
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(host.events(), vec![]);
}

#[tokio::test(flavor = "multi_thread")]
async fn inbound_messages_fire_their_events() {
    let mut server = Server::start().await;
    let (media, _handle, host) = start(&server.url("/"), None, Timeouts::default());
    let conn = server.accept().await;
    host.wait_for(|e| *e == Event::Connect).await;
    for ignored in [
        "not json",
        r#"{"type":"transfer","data":{"to":"x"}}"#,
        r#"{"type":"playAudio","data":{}}"#,
    ] {
        conn.send_text(ignored);
    }
    conn.send_text(r#"{"type":"json","z":1,"a":{"b":[1,2]}}"#);
    conn.send_text(r#"{"no_type": true}"#);
    conn.send_text(r#"{"type":"error","data":{"code":"x"}}"#);
    conn.send_text(r#"{"type":"disconnect"}"#);
    conn.send_text(r#"{"type":"killAudio"}"#);
    host.wait_for(|e| *e == Event::KillAudio).await;
    assert_eq!(
        host.events(),
        vec![
            Event::Connect,
            Event::Json(r#"{"type":"json","z":1,"a":{"b":[1,2]}}"#.to_string()),
            Event::Json(r#"{"no_type":true}"#.to_string()),
            Event::Error(Some(r#"{"code":"x"}"#.to_string())),
            Event::Disconnect(None),
            Event::KillAudio,
        ]
    );
    assert_eq!(*host.breaks.lock().unwrap(), vec!["session".to_string()]);
    media.close();
}

#[tokio::test(flavor = "multi_thread")]
async fn message_over_the_receive_limit_disconnects() {
    let mut server = Server::start().await;
    let (media, _handle, host) = start(&server.url("/"), None, Timeouts::default());
    let conn = server.accept().await;
    host.wait_for(|e| *e == Event::Connect).await;
    conn.send_text(format!(
        r#"{{"type":"json","x":"{}"}}"#,
        "x".repeat(MAX_TEXT_LEN)
    ));
    host.wait_for(|e| *e == Event::Disconnect(None)).await;
    assert_eq!(host.events(), vec![Event::Connect, Event::Disconnect(None)]);
    media.close();
}

fn le_bytes(samples: impl IntoIterator<Item = i16>) -> Vec<u8> {
    samples.into_iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// Polls the write-replace side, as the media thread does every 20 ms, until a frame carries audio.
async fn next_played(media: &mut MediaSide) -> [i16; 160] {
    for _ in 0..1000 {
        let mut frame = [0; 160];
        media.on_write_replace(&mut frame);
        if frame.iter().any(|&s| s != 0) {
            return frame;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("nothing played");
}

#[tokio::test(flavor = "multi_thread")]
async fn binary_without_playout_is_ignored() {
    let mut server = Server::start().await;
    let (media, _handle, host) = start(&server.url("/"), None, Timeouts::default());
    let conn = server.accept().await;
    host.wait_for(|e| *e == Event::Connect).await;
    conn.send_binary(le_bytes([1; 4000]));
    conn.send_text(r#"{"type":"json"}"#);
    host.wait_for(|e| matches!(e, Event::Json(_))).await;
    media.close();
}

#[tokio::test(flavor = "multi_thread")]
async fn playout_starts_after_the_prebuffer_and_kill_audio_discards_it() {
    let mut server = Server::start().await;
    let (mut media, _handle, host) =
        start_with(&server.url("/"), None, Some(8000), Timeouts::default());
    let conn = server.accept().await;
    host.wait_for(|e| *e == Event::Connect).await;
    // one sample short of the first threshold (320 * 4), split mid-sample
    let first = le_bytes(1..=1279);
    conn.send_binary(first[..1001].to_vec());
    conn.send_binary(first[1001..].to_vec());
    conn.send_text(r#"{"type":"json"}"#);
    host.wait_for(|e| matches!(e, Event::Json(_))).await;
    let mut frame = [0; 160];
    media.on_write_replace(&mut frame);
    assert_eq!(frame, [0; 160], "played before the prebuffer was full");

    conn.send_binary(le_bytes(1280..=4000));
    let played = next_played(&mut media).await;
    assert_eq!(played.to_vec(), (1..=160).collect::<Vec<i16>>());

    conn.send_text(r#"{"type":"killAudio"}"#);
    host.wait_for(|e| *e == Event::KillAudio).await;
    let mut frame = [0; 160];
    media.on_write_replace(&mut frame);
    assert_eq!(frame, [0; 160], "buffered audio played after killAudio");
    media.close();
}

/// The first sample of the first frame that carries audio.
async fn first_played(media: &mut MediaSide) -> i16 {
    next_played(media)
        .await
        .into_iter()
        .find(|&s| s != 0)
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn audio_sent_right_after_kill_audio_is_played() {
    let mut server = Server::start().await;
    let (mut media, _handle, host) =
        start_with(&server.url("/"), None, Some(8000), Timeouts::default());
    let conn = server.accept().await;
    host.wait_for(|e| *e == Event::Connect).await;
    // exactly the first threshold: released whole, nothing left in the prebuffer
    conn.send_binary(le_bytes(1..=1280));
    assert_eq!(first_played(&mut media).await, 1);
    // barge-in: the next utterance follows the kill at once, before the media thread's next frame
    conn.send_text(r#"{"type":"killAudio"}"#);
    conn.send_binary(le_bytes(20001..=22000));
    conn.send_text(r#"{"type":"json"}"#);
    host.wait_for(|e| matches!(e, Event::Json(_))).await;
    assert_eq!(first_played(&mut media).await, 20001);
    media.close();
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_audio_drops_what_the_prebuffer_still_holds() {
    let mut server = Server::start().await;
    let (mut media, _handle, host) =
        start_with(&server.url("/"), None, Some(8000), Timeouts::default());
    let conn = server.accept().await;
    host.wait_for(|e| *e == Event::Connect).await;
    conn.send_binary(le_bytes(1..=1280));
    // under the next threshold (640): held back
    conn.send_binary(le_bytes(1281..=1380));
    assert_eq!(first_played(&mut media).await, 1);
    conn.send_text(r#"{"type":"killAudio"}"#);
    host.wait_for(|e| *e == Event::KillAudio).await;
    let mut frame = [0; 160];
    media.on_write_replace(&mut frame);
    conn.send_binary(le_bytes(20001..=21000));
    assert_eq!(
        first_played(&mut media).await,
        20001,
        "the killed stream's tail was played"
    );
    media.close();
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_media_side_ends_the_fork() {
    let mut server = Server::start().await;
    let (media, _handle, host) = start(&server.url("/"), None, Timeouts::default());
    let mut conn = server.accept().await;
    host.wait_for(|e| *e == Event::Connect).await;
    // a start that loses a race, or a panic in CLOSE, drops it without close()
    drop(media);
    assert_eq!(conn.next().await, Some(Frame::Close));
    assert_eq!(conn.next().await, None);
}

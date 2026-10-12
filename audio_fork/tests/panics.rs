//! Panic containment in the simulation tier (D4, T9): an injected panic ends its fork only.

mod common;

use std::time::Duration;

use audio_fork::fork::{Event, PanicIn, Timeouts};
use common::{FRAME, start};
use test_server::Server;
use tokio::time;

#[tokio::test(flavor = "multi_thread")]
async fn task_panic_ends_that_fork_only() {
    let mut server = Server::start().await;
    let (mut one, one_handle, one_host) = start(&server.url("/one"), None, Timeouts::default());
    let _one_conn = server.accept().await;
    let (mut two, _two_handle, two_host) = start(&server.url("/two"), None, Timeouts::default());
    let mut two_conn = server.accept().await;
    one_host.wait_for(|e| *e == Event::Connect).await;
    two_host.wait_for(|e| *e == Event::Connect).await;

    one_handle.inject_panic(PanicIn::Task);
    let Event::Error(Some(body)) = one_host.wait_for(|e| matches!(e, Event::Error(_))).await else {
        unreachable!()
    };
    assert!(body.contains("injected in the socket task"), "{body}");
    // the supervisor ends the fork right after firing the error
    for _ in 0..100 {
        if !one_host.ends.lock().unwrap().is_empty() {
            break;
        }
        time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(one_host.ends.lock().unwrap().len(), 1);

    one.on_read(&FRAME);
    two.on_read(&FRAME);
    assert_eq!(two_conn.audio(320).await.len(), 320);
    assert!(two_host.ends.lock().unwrap().is_empty());
    one.close();
    two.close();
}

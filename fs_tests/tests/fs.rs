//! Real-FreeSWITCH scenarios (docs/testing-plan.md#scenarios). Run in the fs-test image by `just test-fs`.

use fs_tests::{Exception, FreeSwitch, Scenario};

fn main() {
    let scenarios: &[(&str, Scenario)] = &[
        ("load_lifecycle", |fs| Box::pin(load_lifecycle(fs))),
        ("start_rejections", |fs| Box::pin(start_rejections(fs))),
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
    let bugs = call.bugs().await;
    assert!(!bugs.contains("rej"), "bugs left: {bugs}");
    call.hangup().await;
}

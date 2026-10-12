//! Real-FreeSWITCH scenarios (docs/testing-plan.md#scenarios). Run in the fs-test image by `just test-fs`.

use fs_tests::{FreeSwitch, Scenario};

fn main() {
    let scenarios: &[(&str, Scenario)] = &[("load_lifecycle", |fs| Box::pin(load_lifecycle(fs)))];
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

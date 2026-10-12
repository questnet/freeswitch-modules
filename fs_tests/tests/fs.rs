//! Real-FreeSWITCH scenarios (docs/testing-plan.md#scenarios). Run in the fs-test image by `just test-fs`.

use fs_tests::{FreeSwitch, Scenario};

fn main() {
    let scenarios: &[(&str, Scenario)] = &[("module_loads", |fs| Box::pin(module_loads(fs)))];
    fs_tests::run(scenarios);
}

async fn module_loads(fs: &FreeSwitch) {
    let reply = fs.api("load mod_audio_fork").await;
    assert!(reply.starts_with("+OK"), "load: {reply}");
    assert_eq!(fs.api("module_exists mod_audio_fork").await.trim(), "true");
    let reply = fs.api("unload mod_audio_fork").await;
    assert!(reply.starts_with("+OK"), "unload: {reply}");
}

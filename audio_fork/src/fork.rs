//! A fork of one call's audio to one WebSocket server, and the ports to the host.

use std::ffi::CStr;

/// The custom events a fork fires (all `SWITCH_EVENT_CUSTOM`, reserved on load).
pub const SUBCLASSES: [&CStr; 7] = [
    c"mod_audio_fork::connect",
    c"mod_audio_fork::connect_failed",
    c"mod_audio_fork::disconnect",
    c"mod_audio_fork::error",
    c"mod_audio_fork::json",
    c"mod_audio_fork::kill_audio",
    c"mod_audio_fork::buffer_overrun",
];

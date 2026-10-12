//! FreeSWITCH glue for mod_audio_fork: exports the module table that FreeSWITCH looks up by name.

use std::ffi::{CStr, c_char};

use audio_fork::{
    api::{self, ParseError},
    fork::SUBCLASSES,
};
use freeswitch::{
    LOG_ERROR, LOG_NOTICE, ModuleInterface, STATUS_SUCCESS, Session, Status, Stream, sys,
};

const SYNTAX: &CStr = c"<uuid> [start | stop | send_text] [ws-url] [mono | mixed | stereo] [8k | 16k | <rate>] [bugname] [metadata] [true | playback] [true] [bidirectional sample rate]";

#[unsafe(no_mangle)]
pub static mod_audio_fork_module_interface: sys::switch_loadable_module_function_table_t =
    sys::switch_loadable_module_function_table_t {
        switch_api_version: sys::SWITCH_API_VERSION as i32,
        load: Some(load),
        shutdown: Some(shutdown),
        runtime: None,
        flags: 0,
    };

unsafe extern "C" fn load(
    module_interface: *mut *mut sys::switch_loadable_module_interface_t,
    pool: *mut sys::switch_memory_pool_t,
) -> Status {
    freeswitch::guard("load", sys::switch_status_t::SWITCH_STATUS_TERM, || {
        let interface = unsafe { ModuleInterface::create(pool, c"mod_audio_fork") };
        unsafe { *module_interface = interface.as_ptr() };
        for (i, name) in SUBCLASSES.iter().enumerate() {
            if !freeswitch::reserve_subclass(name) {
                log(
                    LOG_ERROR,
                    &format!("cannot reserve event subclass {}", name.to_string_lossy()),
                );
                for reserved in &SUBCLASSES[..i] {
                    freeswitch::free_subclass(reserved);
                }
                return sys::switch_status_t::SWITCH_STATUS_TERM;
            }
        }
        interface.add_api(c"uuid_audio_fork", c"audio_fork API", SYNTAX, api);
        log(LOG_NOTICE, "mod_audio_fork loaded");
        STATUS_SUCCESS
    })
}

unsafe extern "C" fn shutdown() -> Status {
    freeswitch::guard("shutdown", STATUS_SUCCESS, || {
        for name in SUBCLASSES {
            freeswitch::free_subclass(name);
        }
        log(LOG_NOTICE, "mod_audio_fork unloaded");
        STATUS_SUCCESS
    })
}

unsafe extern "C" fn api(
    cmd: *const c_char,
    _session: *mut sys::switch_core_session_t,
    stream: *mut sys::switch_stream_handle_t,
) -> Status {
    // the reply is the API's result; the status only tells FreeSWITCH the command ran
    let mut stream = unsafe { Stream::from_raw(stream) };
    let cmd = (!cmd.is_null()).then(|| unsafe { CStr::from_ptr(cmd) });
    let reply = freeswitch::guard(
        "uuid_audio_fork",
        "-ERR Operation Failed\n".to_string(),
        || run_api(cmd),
    );
    stream.write(&reply);
    STATUS_SUCCESS
}

fn run_api(cmd: Option<&CStr>) -> String {
    const FAILED: &str = "-ERR Operation Failed\n";
    let tokens = cmd
        .map(|c| freeswitch::separate_string(c, b' ', 10))
        .unwrap_or_default();
    let command = match api::parse(&tokens) {
        Ok(command) => command,
        Err(ParseError::Usage(reason)) => {
            log(LOG_ERROR, &format!("uuid_audio_fork: {reason}"));
            return format!("-USAGE: {}\n", SYNTAX.to_string_lossy());
        }
        Err(e @ ParseError::Unsupported(_)) => {
            log(LOG_ERROR, &format!("uuid_audio_fork: {e}"));
            return FAILED.to_string();
        }
    };
    let Some(_session) = Session::locate(&command.session) else {
        log(
            LOG_ERROR,
            &format!("uuid_audio_fork: no session {}", command.session),
        );
        return FAILED.to_string();
    };
    FAILED.to_string()
}

#[track_caller]
fn log(level: freeswitch::LogLevel, msg: &str) {
    let caller = std::panic::Location::caller();
    freeswitch::log(level, None, caller.file(), caller.line(), msg);
}

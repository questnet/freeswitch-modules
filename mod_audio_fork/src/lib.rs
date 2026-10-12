//! FreeSWITCH glue for mod_audio_fork: exports the module table that FreeSWITCH looks up by name,
//! owns the tokio runtime and the registry of live forks, and runs `uuid_audio_fork`.

mod bug;
mod host;
mod log;

use std::{
    collections::HashMap,
    ffi::{CStr, c_char, c_void},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use audio_fork::{
    api::{self, Action, ParseError, Start},
    fork::{Config, Fork, ForkHandle, ForkId, Resample, SUBCLASSES, Timeouts},
};
use freeswitch::{
    MediaBug, ModuleInterface, SMBF_READ_STREAM, SMBF_STEREO, SMBF_WRITE_STREAM, STATUS_SUCCESS,
    Session, Status, Stream, sys,
};
use tokio::runtime::Runtime;

use crate::{
    bug::BugData,
    host::{FsHost, FsResampler},
};

const SYNTAX: &CStr = c"<uuid> [start | stop | send_text] [ws-url] [mono | mixed | stereo] [8k | 16k | <rate>] [bugname] [metadata] [true | playback] [true] [bidirectional sample rate]";
const OK: &str = "+OK Success\n";
const FAILED: &str = "-ERR Operation Failed\n";

/// Created on load, shut down on unload (D6).
static RUNTIME: Mutex<Option<Runtime>> = Mutex::new(None);

/// Live forks, touched on start, stop, send_text, CLOSE and unload, never per frame.
static FORKS: Mutex<Option<HashMap<ForkId, Entry>>> = Mutex::new(None);
static SERIAL: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct Entry {
    serial: u64,
    handle: ForkHandle,
    /// `None` between registering and the bug being attached.
    bug: Option<MediaBug>,
}

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
        log::install();
        let interface = unsafe { ModuleInterface::create(pool, c"mod_audio_fork") };
        unsafe { *module_interface = interface.as_ptr() };
        for (i, name) in SUBCLASSES.iter().enumerate() {
            if !freeswitch::reserve_subclass(name) {
                tracing::error!("cannot reserve event subclass {}", name.to_string_lossy());
                for reserved in &SUBCLASSES[..i] {
                    freeswitch::free_subclass(reserved);
                }
                return sys::switch_status_t::SWITCH_STATUS_TERM;
            }
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .thread_name("mod_audio_fork")
            .enable_all()
            .build()
            .expect("building the tokio runtime");
        *RUNTIME.lock().unwrap() = Some(runtime);
        *FORKS.lock().unwrap() = Some(HashMap::new());
        interface.add_api(c"uuid_audio_fork", c"audio_fork API", SYNTAX, api);
        tracing::info!("mod_audio_fork loaded");
        STATUS_SUCCESS
    })
}

unsafe extern "C" fn shutdown() -> Status {
    freeswitch::guard("shutdown", STATUS_SUCCESS, || {
        // FreeSWITCH must not call into this library once it is unmapped, so every bug goes first
        let bugs: Vec<(ForkId, Option<MediaBug>)> = FORKS
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .iter()
            .map(|(id, e)| (id.clone(), e.bug))
            .collect();
        for (id, bug) in bugs {
            if let (Some(session), Some(bug)) = (Session::locate(&id.session), bug) {
                session.remove_media_bug(bug);
            }
        }
        *FORKS.lock().unwrap() = None;
        if let Some(runtime) = RUNTIME.lock().unwrap().take() {
            runtime.shutdown_timeout(Duration::from_secs(5));
        }
        for name in SUBCLASSES {
            freeswitch::free_subclass(name);
        }
        tracing::info!("mod_audio_fork unloaded");
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
    let reply = freeswitch::guard("uuid_audio_fork", FAILED.to_string(), || run_api(cmd));
    stream.write(&reply);
    STATUS_SUCCESS
}

fn run_api(cmd: Option<&CStr>) -> String {
    let tokens = cmd
        .map(|c| freeswitch::separate_string(c, b' ', 10))
        .unwrap_or_default();
    let command = match api::parse(&tokens) {
        Ok(command) => command,
        Err(ParseError::Usage(reason)) => {
            tracing::error!("uuid_audio_fork: {reason}");
            return format!("-USAGE: {}\n", SYNTAX.to_string_lossy());
        }
        Err(e @ ParseError::Unsupported(_)) => {
            tracing::error!("uuid_audio_fork: {e}");
            return FAILED.to_string();
        }
    };
    let Some(session) = Session::locate(&command.session) else {
        tracing::error!("uuid_audio_fork: no session {}", command.session);
        return FAILED.to_string();
    };
    let result = match command.action {
        Action::Start(start) => start_fork(&session, &command.session, start),
        Action::Stop { bugname, text } => {
            stop_fork(&session, fork_id(&command.session, bugname), text)
        }
        Action::SendText { bugname, text } => send_text(fork_id(&command.session, bugname), text),
    };
    match result {
        Ok(()) => OK.to_string(),
        Err(reason) => {
            tracing::error!(session = %command.session, "uuid_audio_fork: {reason}");
            FAILED.to_string()
        }
    }
}

fn fork_id(session: &str, bugname: String) -> ForkId {
    ForkId {
        session: session.to_string(),
        bugname,
    }
}

fn start_fork(session: &Session, uuid: &str, start: Start) -> Result<(), String> {
    let id = fork_id(uuid, start.bugname.clone());
    if FORKS.lock().unwrap().as_ref().unwrap().contains_key(&id) {
        return Err(format!("{id}: bug already attached"));
    }
    if !session.pre_answer() {
        return Err(format!("{id}: channel cannot be pre-answered"));
    }
    let codec_rate = session
        .read_rate()
        .ok_or_else(|| format!("{id}: channel has no read codec"))?;
    let channels = start.channels();
    let config = Config {
        id: id.clone(),
        url: start.url.clone(),
        channels,
        codec_rate,
        rate: start.rate,
        metadata: start.metadata.clone(),
        timeouts: Timeouts::default(),
    };
    let resampler = (codec_rate != start.rate).then(|| {
        Box::new(FsResampler(freeswitch::Resampler::new(
            codec_rate, start.rate, channels,
        ))) as Box<dyn Resample>
    });
    let runtime = RUNTIME.lock().unwrap().as_ref().unwrap().handle().clone();
    let (media, handle) = Fork::start(config, Arc::new(FsHost), resampler, &runtime)
        .map_err(|e| format!("{id}: metadata: {e}"))?;

    let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
    {
        // registered before the bug exists, so a CLOSE right after attaching finds the entry to remove
        let mut forks = FORKS.lock().unwrap();
        let forks = forks.as_mut().unwrap();
        if forks.contains_key(&id) {
            return Err(format!("{id}: bug already attached"));
        }
        forks.insert(
            id.clone(),
            Entry {
                serial,
                handle,
                bug: None,
            },
        );
    }
    let mut flags = 0;
    if start.reads() {
        flags |= SMBF_READ_STREAM;
    }
    if start.writes() {
        flags |= SMBF_WRITE_STREAM;
    }
    if channels == 2 {
        flags |= SMBF_STEREO;
    }
    let data = Box::into_raw(Box::new(BugData {
        media,
        id: id.clone(),
        serial,
    }));
    match unsafe { session.add_media_bug(&id.bugname, bug::callback, data as *mut c_void, flags) } {
        Some(bug) => {
            if let Some(entry) = FORKS
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .get_mut(&id)
                .filter(|e| e.serial == serial)
            {
                entry.bug = Some(bug);
            }
            tracing::info!(session = %uuid, "{id}: streaming {} Hz from {codec_rate} Hz to {}", start.rate, start.url);
            Ok(())
        }
        None => {
            closed(&id, serial);
            // FreeSWITCH never took the data: no CLOSE will come for it
            unsafe { Box::from_raw(data) }.media.close();
            Err(format!("{id}: adding the media bug failed"))
        }
    }
}

fn stop_fork(session: &Session, id: ForkId, text: Option<String>) -> Result<(), String> {
    let (handle, bug) = {
        let forks = FORKS.lock().unwrap();
        let entry = forks
            .as_ref()
            .unwrap()
            .get(&id)
            .ok_or_else(|| format!("{id}: no such fork"))?;
        (entry.handle.clone(), entry.bug)
    };
    let bug = bug.ok_or_else(|| format!("{id}: still starting"))?;
    tracing::info!(session = %id.session, "{id}: stop");
    handle.stop(text);
    // runs CLOSE, which takes the registry lock: none may be held here; false if a hangup got there first
    session.remove_media_bug(bug);
    Ok(())
}

fn send_text(id: ForkId, text: Option<String>) -> Result<(), String> {
    let handle = {
        let forks = FORKS.lock().unwrap();
        forks
            .as_ref()
            .unwrap()
            .get(&id)
            .ok_or_else(|| format!("{id}: no such fork"))?
            .handle
            .clone()
    };
    match text {
        Some(text) => handle
            .send_text(text)
            .map_err(|e| format!("{id}: send_text: {e}")),
        None => Ok(()),
    }
}

/// The fork's CLOSE: drops its registry entry unless a newer fork took the id meanwhile.
fn closed(id: &ForkId, serial: u64) {
    let mut forks = FORKS.lock().unwrap();
    // unload removes every bug before it drops the registry
    let forks = forks.as_mut().expect("CLOSE after unload");
    if forks.get(id).is_some_and(|e| e.serial == serial) {
        forks.remove(id);
    }
}

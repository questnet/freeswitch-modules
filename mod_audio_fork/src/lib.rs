//! FreeSWITCH glue for mod_audio_fork: exports the module table that FreeSWITCH looks up by name,
//! owns the tokio runtime and the registry of live forks, and runs `uuid_audio_fork`.

mod bug;
mod host;
mod log;

use std::{
    collections::HashMap,
    ffi::{CStr, c_char, c_void},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use audio_fork::{
    api::{self, Action, ParseError, Start},
    fork::{Config, Fork, ForkHandle, ForkId, Resample, Resamplers, SUBCLASSES, Timeouts},
};
use freeswitch::{
    MediaBug, ModuleInterface, SMBF_READ_STREAM, SMBF_STEREO, SMBF_WRITE_REPLACE,
    SMBF_WRITE_STREAM, STATUS_SUCCESS, Session, Status, Stream, sys,
};
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
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

/// Touched on start, stop, send_text, CLOSE and unload, never per frame; `None` while not loaded.
static REGISTRY: Mutex<Option<Registry>> = Mutex::new(None);
static SERIAL: AtomicU64 = AtomicU64::new(0);
/// How long unload waits for FreeSWITCH to close the bugs of calls that are hanging up.
const UNLOAD_WAIT: Duration = Duration::from_secs(10);

#[derive(Debug, Default)]
struct Registry {
    forks: HashMap<ForkId, Entry>,
    /// Set at unload: no new forks, so the ones left can only go.
    unloading: bool,
}

fn registry() -> MappedMutexGuard<'static, Registry> {
    MutexGuard::map(REGISTRY.lock(), |r| r.as_mut().expect("module not loaded"))
}

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
        *RUNTIME.lock() = Some(runtime);
        *REGISTRY.lock() = Some(Registry::default());
        interface.add_api(c"uuid_audio_fork", c"audio_fork API", SYNTAX, api);
        tracing::info!("mod_audio_fork loaded");
        STATUS_SUCCESS
    })
}

unsafe extern "C" fn shutdown() -> Status {
    freeswitch::guard("shutdown", STATUS_SUCCESS, || {
        // FreeSWITCH must not call into this library once it is unmapped, so every bug goes first
        let bugs: Vec<(ForkId, Option<MediaBug>)> = {
            let mut registry = registry();
            registry.unloading = true;
            registry
                .forks
                .iter()
                .map(|(id, e)| (id.clone(), e.bug))
                .collect()
        };
        for (id, bug) in bugs {
            if let (Some(session), Some(bug)) = (Session::locate(&id.session), bug) {
                session.remove_media_bug(bug);
            }
        }
        // a call that is hanging up cannot be located any more; FreeSWITCH closes its bug shortly
        wait_for_closes();
        *REGISTRY.lock() = None;
        if let Some(runtime) = RUNTIME.lock().take() {
            runtime.shutdown_timeout(Duration::from_secs(5));
        }
        for name in SUBCLASSES {
            freeswitch::free_subclass(name);
        }
        tracing::info!("mod_audio_fork unloaded");
        STATUS_SUCCESS
    })
}

fn wait_for_closes() {
    let deadline = Instant::now() + UNLOAD_WAIT;
    loop {
        let left = registry().forks.len();
        if left == 0 {
            return;
        }
        if Instant::now() > deadline {
            tracing::error!(target: "crit", "unloading with {left} forks still attached");
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
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
    #[cfg(feature = "test-hooks")]
    if let Some(reply) = test_hook(&tokens) {
        return reply;
    }
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
    if registry().forks.contains_key(&id) {
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
        playout_rate: start.playout.map(|p| p.rate),
        timeouts: Timeouts::default(),
    };
    let resampler = |from: u32, to: u32, channels: usize| {
        (from != to).then(|| {
            Box::new(FsResampler(freeswitch::Resampler::new(from, to, channels)))
                as Box<dyn Resample>
        })
    };
    let resamplers = Resamplers {
        capture: resampler(codec_rate, start.rate, channels),
        playout: start.playout.and_then(|p| resampler(p.rate, codec_rate, 1)),
    };
    let runtime = RUNTIME.lock().as_ref().unwrap().handle().clone();
    let (media, handle) = Fork::start(config, Arc::new(FsHost), resamplers, &runtime)
        .map_err(|e| format!("{id}: metadata: {e}"))?;

    let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
    {
        // registered before the bug exists, so a CLOSE right after attaching finds the entry to remove
        let mut registry = registry();
        if registry.unloading {
            return Err(format!("{id}: the module is unloading"));
        }
        if registry.forks.contains_key(&id) {
            return Err(format!("{id}: bug already attached"));
        }
        registry.forks.insert(
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
    if start.playout.is_some() {
        flags |= SMBF_WRITE_REPLACE;
    }
    let data = Box::into_raw(Box::new(BugData {
        media,
        id: id.clone(),
        serial,
    }));
    match unsafe { session.add_media_bug(&id.bugname, bug::callback, data as *mut c_void, flags) } {
        Some(bug) => {
            let unloading = {
                let mut registry = registry();
                if let Some(entry) = registry.forks.get_mut(&id).filter(|e| e.serial == serial) {
                    entry.bug = Some(bug);
                }
                registry.unloading
            };
            if unloading {
                // unload began after this fork registered and may have missed its bug
                session.remove_media_bug(bug);
                return Err(format!("{id}: the module is unloading"));
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
        let registry = registry();
        let entry = registry
            .forks
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

/// The bug of a registered fork, once it is attached.
fn bug_of(id: &ForkId) -> Option<MediaBug> {
    registry().forks.get(id).and_then(|e| e.bug)
}

/// `<uuid> test_panic_media|test_panic_task <bugname>` (T9), only in test builds.
#[cfg(feature = "test-hooks")]
fn test_hook(tokens: &[String]) -> Option<String> {
    use audio_fork::fork::PanicIn;
    let [session, hook, bugname] = tokens else {
        return None;
    };
    let place = match hook.as_str() {
        "test_panic_media" => PanicIn::Media,
        "test_panic_task" => PanicIn::Task,
        _ => return None,
    };
    let id = fork_id(session, bugname.clone());
    let handle = registry().forks.get(&id).map(|e| e.handle.clone());
    Some(match handle {
        Some(handle) => {
            handle.inject_panic(place);
            OK.to_string()
        }
        None => FAILED.to_string(),
    })
}

fn send_text(id: ForkId, text: Option<String>) -> Result<(), String> {
    let handle = {
        registry()
            .forks
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
    // unload waits for every CLOSE before it drops the registry
    let mut registry = registry();
    if registry.forks.get(id).is_some_and(|e| e.serial == serial) {
        registry.forks.remove(id);
    }
}

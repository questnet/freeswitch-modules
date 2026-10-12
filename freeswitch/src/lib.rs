//! The parts of the FreeSWITCH core that mod_audio_fork calls (D2): generated bindings in [`sys`]
//! and a thin safe layer over them.

use std::{
    any::Any,
    ffi::{CStr, CString, c_char, c_int, c_void},
    panic::{AssertUnwindSafe, catch_unwind},
    ptr,
};

#[allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code,
    unsafe_op_in_unsafe_fn,
    clippy::all
)]
pub mod sys {
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

pub use sys::switch_log_level_t::Type as LogLevel;
pub use sys::switch_log_level_t::{
    SWITCH_LOG_CRIT as LOG_CRIT, SWITCH_LOG_DEBUG as LOG_DEBUG, SWITCH_LOG_ERROR as LOG_ERROR,
    SWITCH_LOG_INFO as LOG_INFO, SWITCH_LOG_NOTICE as LOG_NOTICE,
    SWITCH_LOG_WARNING as LOG_WARNING,
};
pub use sys::switch_status_t::Type as Status;
pub use sys::switch_status_t::{
    SWITCH_STATUS_FALSE as STATUS_FALSE, SWITCH_STATUS_SUCCESS as STATUS_SUCCESS,
};

/// Runs an `extern "C"` entry point's body, turning a panic into a CRIT log line and `on_panic`.
/// A panic must not unwind into FreeSWITCH (it would abort the whole process, D4).
pub fn guard<T>(what: &str, on_panic: T, f: impl FnOnce() -> T) -> T {
    guard_with(what, |_| on_panic, f)
}

/// [`guard`] whose fallback gets the panic message.
pub fn guard_with<T>(what: &str, on_panic: impl FnOnce(&str) -> T, f: impl FnOnce() -> T) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(payload) => {
            let message = panic_message(&*payload);
            log(
                LOG_CRIT,
                None,
                file!(),
                line!(),
                &format!("panic in {what}: {message}"),
            );
            on_panic(message)
        }
    }
}

pub fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("(non-string panic payload)")
}

/// Writes one line to the FreeSWITCH log; `uuid` tags it with a session like `SWITCH_CHANNEL_UUID_LOG`.
pub fn log(level: LogLevel, uuid: Option<&str>, file: &str, line: u32, msg: &str) {
    let file = c_string(file);
    let uuid = uuid.map(c_string);
    let msg = c_string(msg);
    unsafe {
        sys::switch_log_printf(
            sys::switch_text_channel_t::SWITCH_CHANNEL_ID_LOG,
            file.as_ptr(),
            c"mod_audio_fork".as_ptr(),
            line as c_int,
            uuid.as_ref().map_or(ptr::null(), |u| u.as_ptr()),
            level,
            c"%s\n".as_ptr(),
            msg.as_ptr(),
        );
    }
}

/// Text from FreeSWITCH or for it can carry a NUL only by mistake; it is cut there rather than lost.
fn c_string(s: &str) -> CString {
    let end = s.find('\0').unwrap_or(s.len());
    CString::new(&s[..end]).unwrap()
}

/// Splits like FreeSWITCH's API commands do: `'` groups, `\` escapes, and the last of `max`
/// tokens takes the rest of the line.
pub fn separate_string(input: &CStr, delim: u8, max: usize) -> Vec<String> {
    let mut buf = input.to_bytes_with_nul().to_vec();
    let mut tokens: Vec<*mut c_char> = vec![ptr::null_mut(); max];
    let n = unsafe {
        sys::switch_separate_string(
            buf.as_mut_ptr() as *mut c_char,
            delim as c_char,
            tokens.as_mut_ptr(),
            max as u32,
        )
    };
    tokens[..n as usize]
        .iter()
        .map(|&t| unsafe { CStr::from_ptr(t) }.to_string_lossy().into_owned())
        .collect()
}

/// The module interface FreeSWITCH hands to `load`; interfaces added to it live as long as the module.
#[derive(Debug)]
pub struct ModuleInterface(*mut sys::switch_loadable_module_interface_t);

pub type ApiFunction = unsafe extern "C" fn(
    cmd: *const c_char,
    session: *mut sys::switch_core_session_t,
    stream: *mut sys::switch_stream_handle_t,
) -> Status;

impl ModuleInterface {
    /// # Safety
    /// `pool` must be the pool FreeSWITCH passed to the module's load function.
    pub unsafe fn create(pool: *mut sys::switch_memory_pool_t, name: &'static CStr) -> Self {
        let raw =
            unsafe { sys::switch_loadable_module_create_module_interface(pool, name.as_ptr()) };
        assert!(
            !raw.is_null(),
            "switch_loadable_module_create_module_interface failed"
        );
        ModuleInterface(raw)
    }

    pub fn as_ptr(&self) -> *mut sys::switch_loadable_module_interface_t {
        self.0
    }

    pub fn add_api(
        &self,
        name: &'static CStr,
        desc: &'static CStr,
        syntax: &'static CStr,
        function: ApiFunction,
    ) {
        unsafe {
            let api = sys::switch_loadable_module_create_interface(
                self.0,
                sys::switch_module_interface_name_t::SWITCH_API_INTERFACE,
            ) as *mut sys::switch_api_interface_t;
            assert!(
                !api.is_null(),
                "switch_loadable_module_create_interface failed"
            );
            (*api).interface_name = name.as_ptr();
            (*api).desc = desc.as_ptr();
            (*api).syntax = syntax.as_ptr();
            (*api).function = Some(function);
        }
    }
}

/// The reply stream of an API call.
#[derive(Debug)]
pub struct Stream(*mut sys::switch_stream_handle_t);

impl Stream {
    /// # Safety
    /// `raw` must be the stream FreeSWITCH passed to the API function, used only during that call.
    pub unsafe fn from_raw(raw: *mut sys::switch_stream_handle_t) -> Self {
        Stream(raw)
    }

    pub fn write(&mut self, text: &str) {
        let text = c_string(text);
        unsafe {
            let write = (*self.0)
                .write_function
                .expect("stream without write_function");
            write(self.0, c"%s".as_ptr(), text.as_ptr());
        }
    }
}

/// Owner name the subclasses are reserved and freed under; both calls must use the same one.
const SUBCLASS_OWNER: &CStr = c"mod_audio_fork";

/// Fails if another module owns the subclass; a listener that subscribed first does not count.
pub fn reserve_subclass(name: &CStr) -> bool {
    unsafe {
        sys::switch_event_reserve_subclass_detailed(SUBCLASS_OWNER.as_ptr(), name.as_ptr())
            == STATUS_SUCCESS
    }
}

/// Returns false when a listener still holds the subclass; FreeSWITCH then detaches it itself.
pub fn free_subclass(name: &CStr) -> bool {
    unsafe {
        sys::switch_event_free_subclass_detailed(SUBCLASS_OWNER.as_ptr(), name.as_ptr())
            == STATUS_SUCCESS
    }
}

/// A session located by UUID, read-locked so it cannot be destroyed while this exists.
#[derive(Debug)]
pub struct Session(*mut sys::switch_core_session_t);

impl Session {
    pub fn locate(uuid: &str) -> Option<Session> {
        let uuid = c_string(uuid);
        let raw = unsafe {
            sys::switch_core_session_perform_locate(
                uuid.as_ptr(),
                c"mod_audio_fork".as_ptr(),
                c"locate".as_ptr(),
                0,
            )
        };
        (!raw.is_null()).then(|| Session(raw))
    }

    pub fn as_ptr(&self) -> *mut sys::switch_core_session_t {
        self.0
    }

    fn channel(&self) -> *mut sys::switch_channel_t {
        unsafe { sys::switch_core_session_get_channel(self.0) }
    }

    pub fn pre_answer(&self) -> bool {
        unsafe {
            sys::switch_channel_perform_pre_answer(
                self.channel(),
                c"mod_audio_fork".as_ptr(),
                c"pre_answer".as_ptr(),
                0,
            ) == STATUS_SUCCESS
        }
    }

    /// The rate of the read codec; `None` while the channel has no media set up.
    pub fn read_rate(&self) -> Option<u32> {
        unsafe {
            let codec = sys::switch_core_session_get_read_codec(self.0);
            if codec.is_null() || (*codec).implementation.is_null() {
                return None;
            }
            let mut implementation = sys::switch_codec_implementation_t::default();
            (sys::switch_core_session_get_read_impl(self.0, &mut implementation) == STATUS_SUCCESS)
                .then_some(implementation.actual_samples_per_second)
        }
    }

    /// Interrupts what the channel is playing, like `uuid_break`.
    pub fn set_break(&self) {
        unsafe {
            sys::switch_channel_set_flag_value(
                self.channel(),
                sys::switch_channel_flag_t::CF_BREAK,
                2,
            )
        };
    }

    /// Attaches a media bug. The callback gets `user_data` until CLOSE, which FreeSWITCH runs exactly once
    /// unless this fails, in which case `user_data` was never handed over.
    ///
    /// # Safety
    /// `callback` must accept `user_data` for every callback type until CLOSE.
    pub unsafe fn add_media_bug(
        &self,
        name: &str,
        callback: BugCallback,
        user_data: *mut c_void,
        flags: u32,
    ) -> Option<MediaBug> {
        let name = c_string(name);
        let mut bug = ptr::null_mut();
        let status = unsafe {
            sys::switch_core_media_bug_add(
                self.0,
                name.as_ptr(),
                ptr::null(),
                Some(callback),
                user_data,
                0,
                flags,
                &mut bug,
            )
        };
        (status == STATUS_SUCCESS).then_some(MediaBug(bug))
    }

    /// Removes the bug (running its CLOSE in this thread) if it is still attached; false if it is gone already.
    pub fn remove_media_bug(&self, bug: MediaBug) -> bool {
        // bugs live in the session pool, so a stale pointer is still readable while the session is locked
        let mut raw = bug.0;
        unsafe { sys::switch_core_media_bug_remove(self.0, &mut raw) == STATUS_SUCCESS }
    }

    /// Fires a `SWITCH_EVENT_CUSTOM` event with the channel's data.
    pub fn fire_custom(&self, subclass: &CStr, headers: &[(&CStr, &str)], body: Option<&str>) {
        unsafe {
            let mut event = ptr::null_mut();
            let created = sys::switch_event_create_subclass_detailed(
                c"mod_audio_fork".as_ptr(),
                c"fire_custom".as_ptr(),
                0,
                &mut event,
                sys::switch_event_types_t::SWITCH_EVENT_CUSTOM,
                subclass.as_ptr(),
            );
            assert_eq!(
                created,
                STATUS_SUCCESS,
                "creating event {}",
                subclass.to_string_lossy()
            );
            sys::switch_channel_event_set_data(self.channel(), event);
            for (name, value) in headers {
                let value = c_string(value);
                sys::switch_event_add_header_string(
                    event,
                    sys::switch_stack_t::SWITCH_STACK_BOTTOM,
                    name.as_ptr(),
                    value.as_ptr(),
                );
            }
            if let Some(body) = body {
                let body = c_string(body);
                sys::switch_event_add_body(event, c"%s".as_ptr(), body.as_ptr());
            }
            sys::switch_event_fire_detailed(
                c"mod_audio_fork".as_ptr(),
                c"fire_custom".as_ptr(),
                0,
                &mut event,
                ptr::null_mut(),
            );
            // fire takes the event; it is left behind only if firing failed
            if !event.is_null() {
                sys::switch_event_destroy(&mut event);
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe { sys::switch_core_session_rwunlock(self.0) };
    }
}

pub type BugCallback = unsafe extern "C" fn(
    bug: *mut sys::switch_media_bug_t,
    user_data: *mut c_void,
    abc: sys::switch_abc_type_t::Type,
) -> sys::switch_bool_t::Type;

pub use sys::switch_abc_type_t::{
    SWITCH_ABC_TYPE_CLOSE as ABC_CLOSE, SWITCH_ABC_TYPE_READ as ABC_READ,
    SWITCH_ABC_TYPE_WRITE_REPLACE as ABC_WRITE_REPLACE,
};
pub use sys::switch_bool_t::{SWITCH_FALSE as FALSE, SWITCH_TRUE as TRUE};
pub use sys::switch_media_bug_flag_enum_t::{
    SMBF_READ_STREAM, SMBF_STEREO, SMBF_WRITE_REPLACE, SMBF_WRITE_STREAM,
};

/// A media bug pointer. Valid as long as its session is; whether it is still attached is FreeSWITCH's to say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaBug(*mut sys::switch_media_bug_t);

// only ever dereferenced by FreeSWITCH, with the session locked
unsafe impl Send for MediaBug {}

impl MediaBug {
    /// # Safety
    /// `raw` must be the bug FreeSWITCH passed to the callback that is running.
    pub unsafe fn from_raw(raw: *mut sys::switch_media_bug_t) -> Self {
        MediaBug(raw)
    }

    /// Reads the captured frames available now, at the codec rate, interleaved for stereo.
    /// Only from the bug's READ callback.
    pub fn read_frames(&self, mut each: impl FnMut(&[i16])) {
        let mut buf = [0i16; sys::SWITCH_RECOMMENDED_BUFFER_SIZE as usize / 2];
        loop {
            let mut frame = sys::switch_frame_t {
                data: buf.as_mut_ptr() as *mut c_void,
                buflen: sys::SWITCH_RECOMMENDED_BUFFER_SIZE,
                ..Default::default()
            };
            if unsafe { sys::switch_core_media_bug_read(self.0, &mut frame, TRUE) }
                != STATUS_SUCCESS
            {
                break;
            }
            let samples = frame.datalen as usize / 2;
            if samples > 0 {
                each(&buf[..samples]);
            }
        }
    }
}

impl MediaBug {
    /// Lets `mix` change the frame going to the caller; it is written back only if `mix` says it changed it.
    /// Only from the bug's WRITE_REPLACE callback.
    pub fn replace_write_frame(&self, mix: impl FnOnce(&mut [i16]) -> bool) {
        unsafe {
            let frame = sys::switch_core_media_bug_get_write_replace_frame(self.0);
            let samples = std::slice::from_raw_parts_mut(
                (*frame).data as *mut i16,
                (*frame).samples as usize,
            );
            if mix(samples) {
                (*frame).channels = 1;
                (*frame).datalen = (*frame).samples * 2;
                sys::switch_core_media_bug_set_write_replace_frame(self.0, frame);
            }
        }
    }
}

/// FreeSWITCH's speex resampler for interleaved 16 bit audio (D8).
#[derive(Debug)]
pub struct Resampler {
    raw: *mut sys::switch_audio_resampler_t,
    channels: usize,
}

// owned by one fork and used by one thread at a time
unsafe impl Send for Resampler {}

impl Resampler {
    pub fn new(from_rate: u32, to_rate: u32, channels: usize) -> Self {
        let mut raw = ptr::null_mut();
        let status = unsafe {
            sys::switch_resample_perform_create(
                &mut raw,
                from_rate,
                to_rate,
                sys::SWITCH_RECOMMENDED_BUFFER_SIZE,
                sys::SWITCH_RESAMPLE_QUALITY as c_int,
                channels as u32,
                c"mod_audio_fork".as_ptr(),
                c"resampler".as_ptr(),
                0,
            )
        };
        assert_eq!(
            status, STATUS_SUCCESS,
            "creating a resampler {from_rate} -> {to_rate}"
        );
        Resampler { raw, channels }
    }

    pub fn process(&mut self, input: &[i16], output: &mut Vec<i16>) {
        unsafe {
            // lengths are per channel; FreeSWITCH grows its output buffer as needed and does not write the input
            let frames = (input.len() / self.channels) as u32;
            let out =
                sys::switch_resample_process(self.raw, input.as_ptr() as *mut i16, frames) as usize;
            output.extend_from_slice(std::slice::from_raw_parts(
                (*self.raw).to,
                out * self.channels,
            ));
        }
    }
}

impl Drop for Resampler {
    fn drop(&mut self) {
        unsafe { sys::switch_resample_destroy(&mut self.raw) };
    }
}

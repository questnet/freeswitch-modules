//! The parts of the FreeSWITCH core that mod_audio_fork calls (D2): generated bindings in [`sys`]
//! and a thin safe layer over them.

use std::{
    any::Any,
    ffi::{CStr, CString, c_char, c_int},
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
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(payload) => {
            log(
                LOG_CRIT,
                None,
                file!(),
                line!(),
                &format!("panic in {what}: {}", panic_message(&*payload)),
            );
            on_panic
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

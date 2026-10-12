//! FreeSWITCH glue for mod_audio_fork: exports the module table that FreeSWITCH looks up by name.

use std::ffi::c_char;

use freeswitch::sys;

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
) -> sys::switch_status_t::Type {
    unsafe {
        *module_interface = sys::switch_loadable_module_create_module_interface(
            pool,
            c"mod_audio_fork".as_ptr() as *const c_char,
        );
    }
    sys::switch_status_t::SWITCH_STATUS_SUCCESS
}

unsafe extern "C" fn shutdown() -> sys::switch_status_t::Type {
    sys::switch_status_t::SWITCH_STATUS_SUCCESS
}

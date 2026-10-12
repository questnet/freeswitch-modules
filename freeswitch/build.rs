//! Generates the FreeSWITCH bindings against the headers being built against (D10), so they
//! cannot drift from the FreeSWITCH version the module is loaded into.
//!
//! - `FREESWITCH_INCLUDE_DIR` (required): the include path(s), `:`-separated, e.g. the installed
//!   `/usr/local/freeswitch/include/freeswitch`.
//! - `FREESWITCH_LIB_DIR` (optional): where `libfreeswitch.so` is; links it when set. Unset, the
//!   symbols stay undefined and are resolved by the `freeswitch` process at load time.

use std::{env, path::PathBuf};

/// Only what mod_audio_fork calls (D2); several entry points are macros around these.
const FUNCTIONS: &[&str] = &[
    "switch_loadable_module_create_module_interface",
    "switch_loadable_module_create_interface",
    "switch_core_session_perform_locate",
    "switch_core_session_rwunlock",
    "switch_core_session_get_channel",
    "switch_core_session_get_uuid",
    "switch_core_session_get_read_codec",
    "switch_core_session_get_read_impl",
    "switch_channel_perform_pre_answer",
    "switch_channel_set_private",
    "switch_channel_get_private",
    "switch_channel_set_flag_value",
    "switch_core_media_bug_add",
    "switch_core_media_bug_remove",
    "switch_core_media_bug_get_session",
    "switch_core_media_bug_get_user_data",
    "switch_core_media_bug_read",
    "switch_core_media_bug_get_write_replace_frame",
    "switch_core_media_bug_set_write_replace_frame",
    "switch_event_create_subclass_detailed",
    "switch_channel_event_set_data",
    "switch_event_add_header_string",
    "switch_event_add_body",
    "switch_event_fire_detailed",
    "switch_event_destroy",
    "switch_event_reserve_subclass_detailed",
    "switch_event_free_subclass_detailed",
    "switch_resample_perform_create",
    "switch_resample_process",
    "switch_resample_destroy",
    "switch_log_printf",
    "switch_separate_string",
];

const TYPES: &[&str] = &[
    "switch_loadable_module_function_table_t",
    "switch_api_interface_t",
    "switch_stream_handle_t",
    "switch_frame_t",
    "switch_codec_implementation_t",
    "switch_audio_resampler_t",
    "switch_media_bug_flag_enum_t",
    "switch_channel_flag_t",
    "switch_event_types_t",
    "switch_stack_t",
];

const VARS: &[&str] = &[
    "SWITCH_API_VERSION",
    "SWITCH_RECOMMENDED_BUFFER_SIZE",
    "SWITCH_RESAMPLE_QUALITY",
];

fn main() {
    println!("cargo:rerun-if-env-changed=FREESWITCH_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=FREESWITCH_LIB_DIR");

    let include = env::var_os("FREESWITCH_INCLUDE_DIR")
        .expect("FREESWITCH_INCLUDE_DIR must name the FreeSWITCH include directory (see freeswitch/build.rs)");

    let mut builder = bindgen::Builder::default()
        .header_contents("wrapper.h", "#include <switch.h>\n")
        .use_core()
        .derive_debug(true)
        .derive_default(true)
        .layout_tests(false)
        .default_enum_style(bindgen::EnumVariation::ModuleConsts)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));
    for dir in env::split_paths(&include) {
        builder = builder.clang_arg(format!("-I{}", dir.display()));
    }
    for f in FUNCTIONS {
        builder = builder.allowlist_function(f);
    }
    for t in TYPES {
        builder = builder.allowlist_type(t);
    }
    for v in VARS {
        builder = builder.allowlist_var(v);
    }
    let bindings = builder
        .generate()
        .expect("generating the FreeSWITCH bindings");

    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("bindings.rs");
    bindings
        .write_to_file(&out)
        .expect("writing the FreeSWITCH bindings");

    if let Some(lib) = env::var_os("FREESWITCH_LIB_DIR") {
        println!(
            "cargo:rustc-link-search=native={}",
            PathBuf::from(lib).display()
        );
        println!("cargo:rustc-link-lib=dylib=freeswitch");
    }
}

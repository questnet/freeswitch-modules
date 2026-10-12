//! The parts of the FreeSWITCH core that mod_audio_fork calls (D2): generated bindings in [`sys`]
//! and a thin safe layer over them.

#[allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code,
    clippy::all
)]
pub mod sys {
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

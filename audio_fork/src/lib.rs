//! The mod_audio_fork core. It never names a FreeSWITCH type (testing T1): the glue in
//! `mod_audio_fork` turns FreeSWITCH callbacks into calls on this crate.

pub mod api;
pub mod fork;
pub mod playout;
mod socket;

//! The media bug callback: turns FreeSWITCH's callbacks into calls on the fork's [`MediaSide`].

use std::ffi::c_void;

use audio_fork::fork::{ForkId, MediaSide};
use freeswitch::{ABC_CLOSE, ABC_READ, ABC_WRITE_REPLACE, FALSE, MediaBug, TRUE, sys};

/// The bug's user data, from `start` until CLOSE.
#[derive(Debug)]
pub struct BugData {
    pub media: MediaSide,
    pub id: ForkId,
    /// Tells this fork's registry entry from a later one with the same id.
    pub serial: u64,
}

pub unsafe extern "C" fn callback(
    bug: *mut sys::switch_media_bug_t,
    user_data: *mut c_void,
    abc: sys::switch_abc_type_t::Type,
) -> sys::switch_bool_t::Type {
    let data = user_data as *mut BugData;
    // FALSE makes FreeSWITCH prune the bug, so a panicking fork ends through CLOSE like any other
    freeswitch::guard("media bug callback", FALSE, || match abc {
        ABC_READ => {
            let data = unsafe { &mut *data };
            unsafe { MediaBug::from_raw(bug) }.read_frames(|samples| data.media.on_read(samples));
            TRUE
        }
        ABC_WRITE_REPLACE => {
            let data = unsafe { &mut *data };
            unsafe { MediaBug::from_raw(bug) }
                .replace_write_frame(|frame| data.media.on_write_replace(frame));
            TRUE
        }
        ABC_CLOSE => {
            // CLOSE runs exactly once, after every other callback: the user data is reclaimed here only
            let data = unsafe { Box::from_raw(data) };
            crate::closed(&data.id, data.serial);
            tracing::info!(session = %data.id.session, "{}: bug closed", data.id);
            data.media.close();
            TRUE
        }
        _ => TRUE,
    })
}

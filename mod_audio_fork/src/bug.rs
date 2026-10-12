//! The media bug callback: turns FreeSWITCH's callbacks into calls on the fork's [`MediaSide`].

use std::ffi::c_void;

use audio_fork::fork::{ForkId, MediaSide};
use freeswitch::{ABC_CLOSE, ABC_READ, ABC_WRITE_REPLACE, FALSE, MediaBug, TRUE, sys};
use tracing::info;

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
    let failed = |message: &str| {
        // the media side is left as the panic left it, but its handle to the host is untouched
        if abc != ABC_CLOSE {
            unsafe { &*data }.media.panicked(message);
        }
        FALSE
    };
    freeswitch::guard_with("media bug callback", failed, || match abc {
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
            let BugData { media, id, serial } = *unsafe { Box::from_raw(data) };
            // the fork stops queueing events before a new fork with this bugname can start
            media.on_close();
            crate::unregister(&id, serial);
            info!(session = %id.session, "{id}: bug closed");
            TRUE
        }
        _ => TRUE,
    })
}

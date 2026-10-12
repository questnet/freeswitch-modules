//! The core's ports implemented with FreeSWITCH: events and `CF_BREAK` by session UUID, and resampling.

use audio_fork::fork::{Event, ForkId, Host, Resample};
use freeswitch::{Resampler, Session};
use tracing::debug;

#[derive(Debug)]
pub struct FsHost;

impl Host for FsHost {
    fn fire(&self, fork: &ForkId, event: Event) {
        // a hung-up call has nobody left to tell
        let Some(session) = Session::locate(&fork.session) else {
            return;
        };
        let body = event.body();
        debug!(session = %fork.session, "{fork}: firing {}", event.subclass().to_string_lossy());
        session.fire_custom(
            event.subclass(),
            &[(c"Audio-Fork-Bugname", &fork.bugname)],
            body.as_deref(),
        );
    }

    fn break_playback(&self, session: &str) {
        if let Some(session) = Session::locate(session) {
            session.set_break();
        }
    }

    fn end(&self, fork: &ForkId) {
        let Some(session) = Session::locate(&fork.session) else {
            return;
        };
        if let Some(bug) = crate::bug_of(fork) {
            session.remove_media_bug(bug);
        }
    }
}

#[derive(Debug)]
pub struct FsResampler(pub Resampler);

impl Resample for FsResampler {
    fn process(&mut self, input: &[i16], output: &mut Vec<i16>) {
        self.0.process(input, output);
    }
}

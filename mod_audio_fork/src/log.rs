//! Forwards `tracing` events (the core's and the glue's) to the FreeSWITCH log. A `session` field
//! tags the line with that session's UUID.

use std::fmt::Write;

use tracing::{Event, Level, Subscriber, field::Field};
use tracing_subscriber::{Layer, layer::Context, prelude::*};

pub fn install() {
    // already set when the module is reloaded without the library being unmapped in between
    let _ =
        tracing::subscriber::set_global_default(tracing_subscriber::registry().with(FreeswitchLog));
}

struct FreeswitchLog;

impl<S: Subscriber> Layer<S> for FreeswitchLog {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut line = Line::default();
        event.record(&mut line);
        let meta = event.metadata();
        let level = match *meta.level() {
            Level::ERROR => freeswitch::LOG_ERROR,
            Level::WARN => freeswitch::LOG_WARNING,
            Level::INFO => freeswitch::LOG_INFO,
            Level::DEBUG | Level::TRACE => freeswitch::LOG_DEBUG,
        };
        freeswitch::log(
            level,
            line.session.as_deref(),
            meta.file().unwrap_or("mod_audio_fork"),
            meta.line().unwrap_or(0),
            &line.text,
        );
    }
}

#[derive(Default)]
struct Line {
    text: String,
    session: Option<String>,
}

impl tracing::field::Visit for Line {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        match field.name() {
            "message" => {
                let _ = write!(self.text, "{value:?}");
            }
            "session" => self.session = Some(format!("{value:?}")),
            name => {
                let _ = write!(self.text, " {name}={value:?}");
            }
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "session" => self.session = Some(value.to_string()),
            _ => self.record_debug(field, &value),
        }
    }
}

//! `uuid_audio_fork` arguments, already split into tokens the way FreeSWITCH splits API commands,
//! turned into a [`Command`].
//!
//! Rates are plain integers and only `ws://` / `wss://` URLs are accepted: no caller sends anything else.

use std::fmt;

use tokio_tungstenite::tungstenite::http::Uri;

pub const DEFAULT_BUGNAME: &str = "audio_fork";
/// Upper bound for both sample rates; the capture buffer grows with the rate.
pub const MAX_SAMPLE_RATE: u32 = 64000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub session: String,
    pub action: Action,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Start(Start),
    /// `text` is sent as the final frame before closing.
    Stop {
        bugname: String,
        text: Option<String>,
    },
    /// Without a text it only checks that the fork exists, as the C module did.
    SendText {
        bugname: String,
        text: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Start {
    pub url: Uri,
    pub mix: Mix,
    /// The rate audio is sent to the server at.
    pub rate: u32,
    pub bugname: String,
    pub metadata: Option<String>,
    /// Server audio played to the caller (write-replace); attached only when enable, stream and rate are all set.
    pub playout: Option<Playout>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mix {
    /// The read stream only (what the caller says).
    Mono,
    /// Read and write stream mixed into one channel.
    Mixed,
    /// Read stream in channel 0, write stream in channel 1.
    Stereo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Playout {
    /// The rate the server sends audio at.
    pub rate: u32,
    /// `playback`: the read stream is not forked, only server audio is played.
    pub playback_only: bool,
}

impl Start {
    /// Whether the caller's audio (the read stream) is captured.
    pub fn reads(&self) -> bool {
        !self.playout.is_some_and(|p| p.playback_only)
    }

    /// Whether the audio sent to the caller (the write stream) is captured.
    pub fn writes(&self) -> bool {
        self.mix != Mix::Mono
    }

    pub fn channels(&self) -> usize {
        if self.mix == Mix::Stereo { 2 } else { 1 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Malformed arguments, answered with the usage.
    Usage(String),
    /// A subcommand this module does not have (any more), answered with `-ERR`.
    Unsupported(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Usage(reason) => f.write_str(reason),
            ParseError::Unsupported(sub) => write!(f, "unsupported subcommand {sub}"),
        }
    }
}

pub fn parse(tokens: &[String]) -> Result<Command, ParseError> {
    let usage = |reason: &str| Err(ParseError::Usage(reason.to_string()));
    let [session, sub, args @ ..] = tokens else {
        return usage("expected <uuid> <subcommand>");
    };
    let action = if sub.eq_ignore_ascii_case("start") {
        Action::Start(parse_start(args).map_err(ParseError::Usage)?)
    } else if sub.eq_ignore_ascii_case("stop") {
        let (bugname, text) = bugname_and_text(args);
        Action::Stop { bugname, text }
    } else if sub.eq_ignore_ascii_case("send_text") {
        if args.is_empty() {
            return usage("send_text needs a text");
        }
        let (bugname, text) = bugname_and_text(args);
        Action::SendText { bugname, text }
    } else {
        return Err(ParseError::Unsupported(sub.clone()));
    };
    Ok(Command {
        session: session.clone(),
        action,
    })
}

/// `[bugname] [text]`: two arguments are both, a single one is the text if it looks like JSON.
fn bugname_and_text(args: &[String]) -> (String, Option<String>) {
    match args {
        [] => (DEFAULT_BUGNAME.to_string(), None),
        [one] if looks_like_json(one) => (DEFAULT_BUGNAME.to_string(), Some(one.clone())),
        [one] => (one.clone(), None),
        [bugname, text, ..] => (bugname.clone(), Some(text.clone())),
    }
}

fn looks_like_json(token: &str) -> bool {
    token.starts_with('{') || token.starts_with('[')
}

/// `<url> <mix> <rate> [bugname] [metadata] [bidi enable] [bidi stream] [bidi rate]`
fn parse_start(args: &[String]) -> Result<Start, String> {
    let [url, mix, rate, rest @ ..] = args else {
        return Err("start needs <url> <mix> <rate>".to_string());
    };
    let mut bugname = DEFAULT_BUGNAME.to_string();
    let mut metadata = None;
    let mut playout = None;
    match rest {
        [] => {}
        [one] if looks_like_json(one) => metadata = Some(one.clone()),
        [one] => bugname = one.clone(),
        // the bidi fields only count when all five optional arguments are given
        [name, meta, enable, stream, bidi_rate] => {
            if !name.is_empty() {
                bugname = name.clone();
            }
            if !meta.is_empty() {
                metadata = Some(meta.clone());
            }
            let rate = parse_rate(bidi_rate, 0)?;
            let playback_only = enable == "playback";
            if (playback_only || enable == "true") && stream == "true" && rate != 0 {
                playout = Some(Playout {
                    rate,
                    playback_only,
                });
            }
        }
        [name, meta, ..] => {
            bugname = name.clone();
            metadata = Some(meta.clone()).filter(|m| !m.is_empty());
        }
    }
    let mix = match mix.as_str() {
        "mono" => Mix::Mono,
        "mixed" => Mix::Mixed,
        "stereo" => Mix::Stereo,
        other => {
            return Err(format!(
                "invalid mix type {other}, must be mono, mixed or stereo"
            ));
        }
    };
    let rate = match rate.as_str() {
        "8k" => 8000,
        "16k" => 16000,
        other => parse_rate(other, 1)?,
    };
    if rate % 8000 != 0 {
        return Err(format!(
            "invalid sample rate {rate}, must be a multiple of 8000"
        ));
    }
    Ok(Start {
        url: parse_url(url)?,
        mix,
        rate,
        bugname,
        metadata,
        playout,
    })
}

fn parse_rate(token: &str, min: u32) -> Result<u32, String> {
    token
        .parse::<u32>()
        .ok()
        .filter(|r| (min..=MAX_SAMPLE_RATE).contains(r))
        .ok_or_else(|| format!("invalid sample rate {token}, must be {min}..={MAX_SAMPLE_RATE}"))
}

fn parse_url(token: &str) -> Result<Uri, String> {
    let invalid = || format!("invalid websocket url {token}");
    let uri: Uri = token.parse().map_err(|_| invalid())?;
    let scheme = uri.scheme_str().unwrap_or_default().to_ascii_lowercase();
    if !matches!(scheme.as_str(), "ws" | "wss") || uri.host().is_none_or(str::is_empty) {
        return Err(invalid());
    }
    // tungstenite only connects to a lower-case scheme; `WS://` was accepted by the C module
    let mut parts = uri.into_parts();
    parts.scheme = Some(scheme.parse().unwrap());
    Ok(Uri::from_parts(parts).unwrap())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn tokens(s: &[&str]) -> Vec<String> {
        s.iter().map(|t| t.to_string()).collect()
    }

    /// The C module's branch structure, transcribed with argv indexing (plus the intended
    /// differences: strict rates, ws/wss only), so the parser can be checked against it.
    fn c_rules(argv: &[String]) -> Result<Command, ParseError> {
        let argc = argv.len();
        let usage = || Err(ParseError::Usage(String::new()));
        if argc < 2 || (argv[1].eq_ignore_ascii_case("start") && argc < 5) {
            return usage();
        }
        let mut bugname = DEFAULT_BUGNAME.to_string();
        let json = |t: &String| t.starts_with('{') || t.starts_with('[');
        let action = if argv[1].eq_ignore_ascii_case("stop")
            || argv[1].eq_ignore_ascii_case("send_text")
        {
            let mut text = None;
            if argv[1].eq_ignore_ascii_case("send_text") && argc < 3 {
                return usage();
            }
            if argc > 3 {
                bugname = argv[2].clone();
                text = Some(argv[3].clone());
            } else if argc > 2 {
                if json(&argv[2]) {
                    text = Some(argv[2].clone());
                } else {
                    bugname = argv[2].clone();
                }
            }
            if argv[1].eq_ignore_ascii_case("stop") {
                Action::Stop { bugname, text }
            } else {
                Action::SendText { bugname, text }
            }
        } else if argv[1].eq_ignore_ascii_case("start") {
            let mut metadata = None;
            let mut playout = None;
            let mut bidi_rate = 0;
            if argc > 9 {
                if !argv[5].is_empty() {
                    bugname = argv[5].clone();
                }
                if !argv[6].is_empty() {
                    metadata = Some(argv[6].clone());
                }
                let playback_only = argv[7] == "playback";
                let enable = playback_only || argv[7] == "true";
                let stream = argv[8] == "true";
                let Ok(rate) = argv[9].parse::<u32>() else {
                    return usage();
                };
                bidi_rate = rate;
                if enable && stream && rate != 0 {
                    playout = Some(Playout {
                        rate,
                        playback_only,
                    });
                }
            } else if argc > 6 {
                bugname = argv[5].clone();
                metadata = Some(argv[6].clone()).filter(|m| !m.is_empty());
            } else if argc > 5 {
                if json(&argv[5]) {
                    metadata = Some(argv[5].clone());
                } else {
                    bugname = argv[5].clone();
                }
            }
            let mix = match argv[3].as_str() {
                "mixed" => Mix::Mixed,
                "stereo" => Mix::Stereo,
                "mono" => Mix::Mono,
                _ => return usage(),
            };
            let rate = match argv[4].as_str() {
                "16k" => 16000,
                "8k" => 8000,
                other => match other.parse::<u32>() {
                    Ok(r) => r,
                    Err(_) => return usage(),
                },
            };
            let Ok(url) = argv[2].parse::<Uri>() else {
                return usage();
            };
            let scheme = url.scheme_str().unwrap_or_default().to_ascii_lowercase();
            if !matches!(scheme.as_str(), "ws" | "wss") || url.host().is_none_or(str::is_empty) {
                return usage();
            }
            if rate == 0
                || rate > MAX_SAMPLE_RATE
                || rate % 8000 != 0
                || bidi_rate > MAX_SAMPLE_RATE
            {
                return usage();
            }
            Action::Start(Start {
                url,
                mix,
                rate,
                bugname,
                metadata,
                playout,
            })
        } else {
            return Err(ParseError::Unsupported(argv[1].clone()));
        };
        Ok(Command {
            session: argv[0].clone(),
            action,
        })
    }

    fn one_of(tokens: &'static [&'static str]) -> BoxedStrategy<String> {
        prop::sample::select(tokens)
            .prop_map(str::to_string)
            .boxed()
    }

    /// One vocabulary per position.
    fn positions(
        vocabularies: &[&'static [&'static str]],
        lengths: std::ops::RangeInclusive<usize>,
    ) -> BoxedStrategy<Vec<String>> {
        let positions: Vec<_> = vocabularies.iter().map(|v| one_of(v)).collect();
        lengths
            .prop_flat_map(move |n| positions[..n].to_vec())
            .boxed()
    }

    /// Mostly well-formed commands of each shape, so that every branch comes up often, plus junk.
    fn argv() -> impl Strategy<Value = Vec<String>> {
        let start = positions(
            &[
                &["uuid"],
                &["start", "START"],
                &[
                    "ws://host/path",
                    "wss://host:8443",
                    "WS://host",
                    "http://host/",
                    "ws://",
                ],
                &["mono", "mixed", "stereo", "Mono"],
                &["8k", "16k", "8000", "24000", "64000", "72000", "0", "8000x"],
                &["bug", "", "{\"a\":1}", "[1]"],
                &["{\"a\":1}", "", "bug"],
                &["true", "playback", "false", ""],
                &["true", "false", ""],
                &["0", "8000", "16000", "72000", "x", ""],
            ],
            5..=10,
        );
        let text = &["bug", "", "{\"a\":1}", "[1]", "x"][..];
        let text = positions(
            &[
                &["uuid"],
                &["stop", "STOP", "send_text", "Send_Text"],
                text,
                text,
                text,
            ],
            2..=5,
        );
        let all = &[
            "",
            "uuid",
            "start",
            "stop",
            "send_text",
            "pause",
            "mono",
            "8k",
            "bug",
            "{}",
            "ws://h/",
            "true",
        ][..];
        let junk = positions(&[all; 10], 0..=10);
        prop_oneof![start, text, junk]
    }

    proptest! {
        // the rarest branches (all ten start tokens valid) come up about once in 200 cases
        #![proptest_config(ProptestConfig::with_cases(10_000))]
        #[test]
        fn parse_follows_the_c_rules(argv in argv()) {
            let strip = |r: Result<Command, ParseError>| r.map_err(|e| match e {
                ParseError::Usage(_) => ParseError::Usage(String::new()),
                other => other,
            });
            prop_assert_eq!(strip(parse(&argv)), c_rules(&argv));
        }
    }

    #[test]
    fn short_start_never_plays_out() {
        let cmd = parse(&tokens(&[
            "u", "start", "ws://h/", "mono", "8k", "bug", "{}", "true",
        ]))
        .unwrap();
        let Action::Start(start) = cmd.action else {
            panic!()
        };
        assert_eq!(start.playout, None);
    }

    #[test]
    fn playback_mode_stops_reading_the_caller() {
        let argv = tokens(&[
            "u", "start", "ws://h/", "mono", "8k", "bug", "", "playback", "true", "16000",
        ]);
        let Action::Start(start) = parse(&argv).unwrap().action else {
            panic!()
        };
        assert_eq!(start.metadata, None);
        assert_eq!(
            start.playout,
            Some(Playout {
                rate: 16000,
                playback_only: true
            })
        );
        assert!(!start.reads());
    }

    #[test]
    fn schemes_are_lower_cased_for_tungstenite() {
        let argv = tokens(&["u", "start", "WSS://host/p", "mono", "8k"]);
        let Action::Start(start) = parse(&argv).unwrap().action else {
            panic!()
        };
        assert_eq!(start.url.scheme_str(), Some("wss"));
    }

    #[test]
    fn rates_are_strict_integers() {
        for rate in ["8000x", " 8000", "4294975296"] {
            let argv = tokens(&["u", "start", "ws://h/", "mono", rate]);
            assert!(matches!(parse(&argv), Err(ParseError::Usage(_))), "{rate}");
        }
    }
}

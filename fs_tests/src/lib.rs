//! Harness for running mod_audio_fork against a real FreeSWITCH (docs/testing-plan.md#real-fs-tier).
//!
//! One FreeSWITCH process per run, configured from `tests/fs-conf` and pointed at a mod dir that
//! holds the stock modules plus only the mod_audio_fork variant under test (`AUDIO_FORK_MODULE`).

use std::{
    env, fs,
    future::Future,
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use freeswitch_esl_tokio::{EslClient, EslEventStream, EventFormat};
use futures_util::FutureExt;
use tokio::{
    process::{Child, Command},
    sync::Notify,
};

/// Which build of the module a run loads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    Rust,
    /// Built with `test-hooks`; runs only the scenarios that need them.
    RustHooks,
}

impl Variant {
    pub fn from_env() -> Self {
        match env::var("AUDIO_FORK_MODULE").as_deref() {
            Ok("rust") | Err(_) => Variant::Rust,
            Ok("rust-hooks") => Variant::RustHooks,
            Ok(other) => panic!("AUDIO_FORK_MODULE must be rust or rust-hooks, not {other}"),
        }
    }

    fn dir_name(self) -> &'static str {
        match self {
            Variant::Rust => "rust",
            Variant::RustHooks => "rust-hooks",
        }
    }
}

/// Paths of the installed FreeSWITCH and the module builds, from the environment of the fs-test image.
#[derive(Debug)]
struct Layout {
    freeswitch: PathBuf,
    stock_mod_dir: PathBuf,
    module_dir: PathBuf,
    conf_dir: PathBuf,
}

impl Layout {
    fn from_env() -> Self {
        let var = |name: &str| {
            PathBuf::from(env::var_os(name).unwrap_or_else(|| panic!("{name} must be set")))
        };
        Layout {
            freeswitch: var("FREESWITCH_BIN"),
            stock_mod_dir: var("FREESWITCH_MOD_DIR"),
            module_dir: var("AUDIO_FORK_MODULE_DIR"),
            conf_dir: var("FS_TEST_CONF"),
        }
    }
}

pub struct FreeSwitch {
    pub variant: Variant,
    pub client: EslClient,
    events: Arc<Events>,
    allowed_crits: Mutex<Vec<String>>,
    /// Holds the generated audio files.
    files: PathBuf,
    child: Child,
    log_file: PathBuf,
    // kept alive for the lifetime of the process, removed on drop
    _base: TempDir,
}

impl std::fmt::Debug for FreeSwitch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FreeSwitch")
            .field("variant", &self.variant)
            .finish_non_exhaustive()
    }
}

impl FreeSwitch {
    /// Starts FreeSWITCH, connects over ESL and loads the variant's module.
    pub async fn start(variant: Variant) -> Self {
        let layout = Layout::from_env();
        let base = TempDir::new();
        let dirs = [
            "log",
            "db",
            "run",
            "temp",
            "mod",
            "conf",
            "storage",
            "recordings",
        ];
        for d in dirs {
            fs::create_dir_all(base.0.join(d)).unwrap();
        }
        fs::copy(
            layout.conf_dir.join("freeswitch.xml"),
            base.0.join("conf/freeswitch.xml"),
        )
        .unwrap();
        let mod_dir = base.0.join("mod");
        for entry in fs::read_dir(&layout.stock_mod_dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "so") {
                symlink(&path, &mod_dir.join(path.file_name().unwrap()));
            }
        }
        let module = layout
            .module_dir
            .join(variant.dir_name())
            .join("mod_audio_fork.so");
        assert!(module.exists(), "{} is missing", module.display());
        symlink(&module, &mod_dir.join("mod_audio_fork.so"));

        let log_file = base.0.join("log/freeswitch.log");
        let stdout = fs::File::create(base.0.join("log/stdout.log")).unwrap();
        let mut cmd = Command::new(&layout.freeswitch);
        cmd.args(["-nf", "-nc", "-nonat"]);
        for (flag, dir) in [
            ("-conf", "conf"),
            ("-log", "log"),
            ("-db", "db"),
            ("-run", "run"),
            ("-temp", "temp"),
            ("-mod", "mod"),
            ("-storage", "storage"),
            ("-recordings", "recordings"),
        ] {
            cmd.arg(flag).arg(base.0.join(dir));
        }
        cmd.stdin(Stdio::null())
            .stdout(stdout.try_clone().unwrap())
            .stderr(stdout)
            .kill_on_drop(true);
        let mut child = cmd.spawn().expect("spawning freeswitch");

        let files = base.0.join("storage");
        for rate in [8000, 16000] {
            fs::write(
                files.join(format!("counter.r{}", rate / 1000)),
                counter_file(rate),
            )
            .unwrap();
        }
        let (client, stream) = connect_esl(&mut child).await;
        let subclasses = SUBCLASSES.join(" ");
        client
            .subscribe_events_raw(
                EventFormat::Plain,
                &format!("CHANNEL_EXECUTE_COMPLETE CUSTOM {subclasses}"),
            )
            .await
            .unwrap();
        let events = Arc::new(Events::default());
        tokio::spawn(collect(stream, events.clone()));
        let fs = FreeSwitch {
            variant,
            client,
            events,
            allowed_crits: Mutex::new(Vec::new()),
            files,
            child,
            log_file,
            _base: base,
        };
        fs.api_ok("load mod_audio_fork").await;
        fs
    }

    /// CPU time FreeSWITCH has used so far, user and system.
    pub fn cpu_time(&self) -> Duration {
        let pid = self.child.id().expect("freeswitch has exited");
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        // the command name may contain spaces; the fields after it are space separated
        let fields: Vec<&str> = stat[stat.rfind(')').unwrap() + 2..].split(' ').collect();
        let ticks: u64 = fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap();
        // USER_HZ, 100 on every Linux the image runs on
        Duration::from_millis(ticks * 10)
    }

    /// The number of `mod_audio_fork::buffer_overrun` events so far.
    pub fn overruns(&self) -> usize {
        self.events
            .all()
            .iter()
            .filter(|e| e.subclass == "mod_audio_fork::buffer_overrun")
            .count()
    }

    /// Runs an API command that must reply `+OK`.
    pub async fn api_ok(&self, command: &str) {
        let reply = self.api(command).await;
        assert!(reply.starts_with("+OK"), "{command}: {reply}");
    }

    /// Runs an API command and returns its reply body.
    pub async fn api(&self, command: &str) -> String {
        let resp = self
            .client
            .api(command)
            .await
            .unwrap_or_else(|e| panic!("api {command}: {e}"));
        resp.body().unwrap_or_default().to_string()
    }

    /// A CRIT line containing `text` is expected (an injected panic) and does not fail the run.
    pub fn allow_crit(&self, text: &str) {
        self.allowed_crits.lock().unwrap().push(text.to_string());
    }

    /// Originates a loopback pair with L16 at `rate`; leg B answers and parks, leg A parks.
    pub async fn call(&self, rate: u32) -> Call<'_> {
        let a = self.api("create_uuid").await.trim().to_string();
        // mod_loopback reads this before originate sets the channel's variables, so only a global one counts
        self.api_ok(&format!("global_setvar loopback_initial_codec=L16@{rate}h"))
            .await;
        self.api_ok(&format!(
            "originate {{origination_uuid={a}}}loopback/answer/default &park"
        ))
        .await;
        let b = self
            .api(&format!("uuid_getvar {a} other_loopback_leg_uuid"))
            .await
            .trim()
            .to_string();
        Call {
            fs: self,
            a,
            b,
            rate,
        }
    }

    /// Waits for an event of a fork; every wait is bounded.
    pub async fn event(&self, session: &str, bugname: &str, subclass: &str) -> FsEvent {
        let wanted =
            |e: &FsEvent| e.session == session && e.bugname == bugname && e.subclass == subclass;
        let wait = async {
            loop {
                let fired = self.events.fired.notified();
                if let Some(e) = self.events.all().into_iter().find(wanted) {
                    return e;
                }
                fired.await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), wait)
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "no {subclass} for {session}/{bugname}; got {:?}",
                    self.events_of(session)
                )
            })
    }

    /// Every collected event of one session so far.
    pub fn events_all(&self, session: &str) -> Vec<FsEvent> {
        self.events
            .all()
            .into_iter()
            .filter(|e| e.session == session)
            .collect()
    }

    /// The `mod_audio_fork::*` events of one session so far.
    pub fn events_of(&self, session: &str) -> Vec<FsEvent> {
        self.events
            .all()
            .into_iter()
            .filter(|e| e.session == session && e.subclass.starts_with("mod_audio_fork::"))
            .collect()
    }

    /// Shuts FreeSWITCH down and fails if it crashed or logged a CRIT line.
    pub async fn stop(mut self) {
        // the reply may never come: the socket closes during shutdown
        let _ =
            tokio::time::timeout(Duration::from_secs(2), self.client.api("fsctl shutdown")).await;
        let status = tokio::time::timeout(Duration::from_secs(30), self.child.wait())
            .await
            .expect("freeswitch did not exit within 30 s")
            .unwrap();
        let log = fs::read_to_string(&self.log_file).unwrap_or_default();
        if env::var_os("FS_TESTS_DUMP_LOG").is_some() {
            println!("---- freeswitch.log ----\n{log}---- end of freeswitch.log ----");
        }
        let allowed = self.allowed_crits.lock().unwrap().clone();
        let crits: Vec<&str> = log
            .lines()
            .filter(|l| l.contains("[CRIT]") && !allowed.iter().any(|a| l.contains(a.as_str())))
            .collect();
        assert!(status.success(), "freeswitch exited with {status}");
        assert!(
            crits.is_empty(),
            "freeswitch logged CRIT:\n{}",
            crits.join("\n")
        );
    }
}

#[derive(Debug)]
pub struct Call<'a> {
    fs: &'a FreeSwitch,
    /// The leg the fork is attached to.
    pub a: String,
    /// The other leg: what it plays, leg A reads.
    pub b: String,
    pub rate: u32,
}

impl Call<'_> {
    /// `uuid_audio_fork <a> <args>`, which must succeed.
    pub async fn fork(&self, args: &str) {
        self.fs
            .api_ok(&format!("uuid_audio_fork {} {args}", self.a))
            .await;
    }

    /// Leg B plays a running sample counter starting at 1 (see [`audio::assert_counter`]).
    pub async fn play_counter(&self) {
        let file = self.fs.files.join(format!("counter.r{}", self.rate / 1000));
        self.fs
            .api_ok(&format!(
                "uuid_broadcast {} {} aleg",
                self.b,
                file.display()
            ))
            .await;
    }

    /// Leg A plays digital silence, so its write stream has frames: a bug that captures both
    /// streams delivers nothing while either one is empty.
    pub async fn play_silence_to_a(&self) {
        self.fs
            .api_ok(&format!(
                "uuid_broadcast {} silence_stream://-1,0 aleg",
                self.a
            ))
            .await;
    }

    /// Starts recording what leg B hears, which is what leg A sends.
    pub async fn record_b(&self) -> PathBuf {
        let file = self
            .fs
            .files
            .join(format!("{}.r{}", self.b, self.rate / 1000));
        self.fs
            .api_ok(&format!("uuid_setvar {} RECORD_READ_ONLY true", self.b))
            .await;
        self.fs
            .api_ok(&format!("uuid_record {} start {}", self.b, file.display()))
            .await;
        file
    }

    /// Like [`Call::record_b`]; also returns when the recording started.
    pub async fn record_b_timed(&self) -> (PathBuf, std::time::Instant) {
        let file = self.record_b().await;
        (file, std::time::Instant::now())
    }

    /// Stops the recording and returns its samples.
    pub async fn recorded_b(&self, file: &Path) -> Vec<i16> {
        self.fs
            .api_ok(&format!("uuid_record {} stop {}", self.b, file.display()))
            .await;
        // the recording thread closes the file after the bug is gone
        tokio::time::sleep(Duration::from_millis(200)).await;
        audio::samples(&fs::read(file).unwrap())
    }

    /// Leg B plays a sine tone.
    pub async fn play_tone(&self, hz: u32) {
        self.fs
            .api_ok(&format!(
                "uuid_broadcast {} tone_stream://%(60000,0,{hz}) aleg",
                self.b
            ))
            .await;
    }

    /// The media bugs on leg A, as `uuid_buglist` prints them.
    pub async fn bugs(&self) -> String {
        self.fs.api(&format!("uuid_buglist {}", self.a)).await
    }

    pub async fn hangup(self) {
        self.fs.api_ok(&format!("uuid_kill {}", self.a)).await;
    }
}

/// A `mod_audio_fork::*` event as received over ESL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsEvent {
    /// The subclass of a custom event, otherwise the event name.
    pub subclass: String,
    /// Of `CHANNEL_EXECUTE_COMPLETE`.
    pub application: Option<String>,
    pub bugname: String,
    /// `Unique-ID`, from the channel data.
    pub session: String,
    pub channel_name: Option<String>,
    pub body: Option<String>,
}

const SUBCLASSES: [&str; 7] = [
    "mod_audio_fork::connect",
    "mod_audio_fork::connect_failed",
    "mod_audio_fork::disconnect",
    "mod_audio_fork::error",
    "mod_audio_fork::json",
    "mod_audio_fork::kill_audio",
    "mod_audio_fork::buffer_overrun",
];

#[derive(Debug, Default)]
struct Events {
    received: Mutex<Vec<FsEvent>>,
    fired: Notify,
}

impl Events {
    fn all(&self) -> Vec<FsEvent> {
        self.received.lock().unwrap().clone()
    }
}

async fn collect(mut stream: EslEventStream, events: Arc<Events>) {
    while let Some(Ok(event)) = stream.recv().await {
        let header = |name: &str| event.header_str(name).map(str::to_string);
        let Some(subclass) = header("Event-Subclass").or_else(|| header("Event-Name")) else {
            continue;
        };
        events.received.lock().unwrap().push(FsEvent {
            subclass,
            application: header("Application"),
            bugname: header("Audio-Fork-Bugname").unwrap_or_default(),
            session: header("Unique-ID").unwrap_or_default(),
            channel_name: header("Channel-Name"),
            body: event.body().map(str::to_string),
        });
        events.fired.notify_waiters();
    }
}

/// 60 s of a running 16 bit counter starting at 1, as raw little-endian L16.
fn counter_file(rate: u32) -> Vec<u8> {
    (1..=rate * 60)
        .flat_map(|i| (i as u16).to_le_bytes())
        .collect()
}

/// Checks on received audio.
pub mod audio {
    /// Little-endian L16 bytes as samples.
    pub fn samples(bytes: &[u8]) -> Vec<i16> {
        bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|s| i16::from_le_bytes(*s))
            .collect()
    }

    /// One channel of interleaved audio.
    pub fn channel(samples: &[i16], channels: usize, index: usize) -> Vec<i16> {
        samples
            .iter()
            .skip(index)
            .step_by(channels)
            .copied()
            .collect()
    }

    /// From where the counter starts, every sample is one more than the one before: nothing was
    /// dropped, repeated or reordered. Samples of -1 are skipped: that is FreeSWITCH's silence
    /// before playback and the comfort-noise frames mod_loopback inserts when the other leg is a
    /// little late (the counter only reaches -1 after 65535 samples). Silence after it is where
    /// the counter ended. Returns how many counter samples there were.
    pub fn assert_counter(samples: &[i16]) -> usize {
        let consecutive = |w: &[i16]| w.windows(2).all(|p| p[1] == p[0].wrapping_add(1));
        let start = samples
            .windows(16)
            .position(consecutive)
            .unwrap_or_else(|| {
                let head = &samples[..samples.len().min(32)];
                panic!("no counter in {} samples, starting {head:?}", samples.len())
            });
        let mut counter: Vec<i16> = samples[start..]
            .iter()
            .copied()
            .filter(|&s| s != -1)
            .collect();
        let end = counter.iter().rposition(|&s| s != 0).map_or(0, |i| i + 1);
        counter.truncate(end);
        for (i, pair) in counter.windows(2).enumerate() {
            let around = &counter[i.saturating_sub(10)..(i + 30).min(counter.len())];
            assert_eq!(
                pair[1],
                pair[0].wrapping_add(1),
                "gap after counter sample {i} of {}: {around:?}",
                counter.len()
            );
        }
        counter.len()
    }

    /// The dominant frequency, from the zero crossings between the silence before and after.
    pub fn frequency(samples: &[i16], rate: u32) -> f64 {
        let loud = |s: &i16| s.unsigned_abs() > 1000;
        let start = samples.iter().position(loud).expect("only silence");
        let end = samples.iter().rposition(loud).unwrap() + 1;
        let tone = &samples[start..end];
        let crossings = tone.windows(2).filter(|w| (w[0] < 0) != (w[1] < 0)).count();
        crossings as f64 / 2.0 / (tone.len() as f64 / rate as f64)
    }
}

async fn connect_esl(child: &mut Child) -> (EslClient, EslEventStream) {
    for _ in 0..300 {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("freeswitch exited during startup with {status}");
        }
        if let Ok(conn) = EslClient::connect("127.0.0.1", 8021, "ClueCon").await {
            return conn;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("no ESL connection within 30 s");
}

fn symlink(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link)
        .unwrap_or_else(|e| panic!("symlink {}: {e}", link.display()));
}

#[derive(Debug)]
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = env::temp_dir().join(format!("fs_tests-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub type Scenario = for<'a> fn(&'a FreeSwitch) -> Pin<Box<dyn Future<Output = ()> + 'a>>;

/// Scenarios that need the `test-hooks` build run only against it, all others never.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Needs {
    Any,
    Hooks,
}

/// Runs the scenarios whose name contains the first command-line argument (all without one)
/// against one FreeSWITCH, and exits non-zero if any failed or FreeSWITCH did not stop cleanly.
/// Starts FreeSWITCH for `run_with` and stops it after `f`, failing on a crash or CRIT.
pub fn with_freeswitch<T>(
    f: impl for<'a> FnOnce(&'a FreeSwitch) -> Pin<Box<dyn Future<Output = T> + 'a>>,
) -> T {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let fs = FreeSwitch::start(Variant::from_env()).await;
        let result = f(&fs).await;
        fs.stop().await;
        result
    })
}

pub fn run(scenarios: &[(&str, Needs, Scenario)]) {
    let filter = env::args().nth(1).filter(|a| !a.starts_with('-'));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let failed = runtime.block_on(async {
        let variant = Variant::from_env();
        let fs = FreeSwitch::start(variant).await;
        let mut failed = Vec::new();
        for (name, needs, scenario) in scenarios {
            if filter.as_deref().is_some_and(|f| !name.contains(f))
                || (*needs == Needs::Hooks) != (variant == Variant::RustHooks)
            {
                continue;
            }
            println!("scenario {name} ({variant:?}) ...");
            match AssertUnwindSafe(scenario(&fs)).catch_unwind().await {
                Ok(()) => println!("scenario {name} ok"),
                Err(_) => {
                    println!("scenario {name} FAILED");
                    failed.push(*name);
                }
            }
        }
        fs.stop().await;
        failed
    });
    if !failed.is_empty() {
        eprintln!("failed scenarios: {}", failed.join(", "));
        std::process::exit(1);
    }
}

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
    time::Duration,
};

use freeswitch_esl_tokio::{EslClient, EslEventStream};
use futures_util::FutureExt;
use tokio::process::{Child, Command};

/// Which build of the module a run loads (differential tests, T4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    Rust,
    C,
}

impl Variant {
    pub fn from_env() -> Self {
        match env::var("AUDIO_FORK_MODULE").as_deref() {
            Ok("rust") | Err(_) => Variant::Rust,
            Ok("c") => Variant::C,
            Ok(other) => panic!("AUDIO_FORK_MODULE must be rust or c, not {other}"),
        }
    }

    fn dir_name(self) -> &'static str {
        match self {
            Variant::Rust => "rust",
            Variant::C => "c",
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
    pub events: EslEventStream,
    child: Child,
    log_file: PathBuf,
    // kept alive for the lifetime of the process, removed on drop
    _base: TempDir,
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

        let (client, events) = connect_esl(&mut child).await;
        let fs = FreeSwitch {
            variant,
            client,
            events,
            child,
            log_file,
            _base: base,
        };
        fs.api_ok("load mod_audio_fork").await;
        fs
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
        let crits: Vec<&str> = log.lines().filter(|l| l.contains("[CRIT]")).collect();
        assert!(status.success(), "freeswitch exited with {status}");
        assert!(
            crits.is_empty(),
            "freeswitch logged CRIT:\n{}",
            crits.join("\n")
        );
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

/// Runs the scenarios whose name contains the first command-line argument (all without one)
/// against one FreeSWITCH, and exits non-zero if any failed or FreeSWITCH did not stop cleanly.
pub fn run(scenarios: &[(&str, Scenario)]) {
    let filter = env::args().nth(1).filter(|a| !a.starts_with('-'));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let failed = runtime.block_on(async {
        let variant = Variant::from_env();
        let fs = FreeSwitch::start(variant).await;
        let mut failed = Vec::new();
        for (name, scenario) in scenarios {
            if filter.as_deref().is_some_and(|f| !name.contains(f)) {
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

//! lino (https://github.com/astralyxdev/lino, MIT): an indexed file editor
//! for agents, with a live process per folder that keeps the index warm.
//!
//! xharness ships its own lino. build.rs clones it at a pinned version and
//! builds it for the platform being built; the binaries are compiled into
//! the app and unpacked to the data directory the first time they're
//! needed. Every project added gets lino turned on for its folder,
//! terminals find this lino first on `PATH`, and the Lino window shows what
//! the live process is doing.
//!
//! Everything here runs `lino <command> --json`, which blocks: call it off
//! the main thread.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::OnceLock,
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::services::rpc::{handle_call, route};

/// The bundled lino's version (its tag), from build.rs.
#[cfg(lino_bundled)]
pub const VERSION: &str = env!("LINO_VERSION");
#[cfg(not(lino_bundled))]
pub const VERSION: &str = "none";
#[cfg(lino_bundled)]
const COMMIT: &str = env!("LINO_COMMIT");
#[cfg(not(lino_bundled))]
const COMMIT: &str = "none";

/// lino is a thin client that execs `lino-core` from its own folder, so the
/// two always go together.
struct Bundle {
    lino: &'static [u8],
    core: &'static [u8],
}

#[cfg(lino_bundled)]
static BUNDLE: Option<Bundle> = Some(Bundle {
    lino: include_bytes!(concat!(env!("LINO_DIR"), "/lino", env!("LINO_EXE"))),
    core: include_bytes!(concat!(env!("LINO_DIR"), "/lino-core", env!("LINO_EXE"))),
});

/// Built with XHARNESS_SKIP_LINO: use a lino on `PATH`, if there is one.
#[cfg(not(lino_bundled))]
static BUNDLE: Option<Bundle> = None;

/// The marker of a folder lino is set up in.
const LINO_DIR: &str = ".lino";

/// A lino to run.
pub struct Lino {
    path: PathBuf,
    bundled: bool,
}

impl Lino {
    /// The bundled lino (unpacked on first use), or else one on `PATH`.
    pub fn get() -> Option<&'static Lino> {
        static LINO: OnceLock<Option<Lino>> = OnceLock::new();
        LINO.get_or_init(|| {
            let bundled = BUNDLE.as_ref().zip(crate::services::paths::data_dir()).and_then(|(bundle, data)| {
                let dir = data.join("lino").join(format!("{VERSION}-{COMMIT}"));
                install(&dir, bundle)
                    .inspect_err(|error| eprintln!("xharness: can't unpack lino to {}: {error}", dir.display()))
                    .ok()
            });
            match bundled {
                Some(path) => Some(Lino { path, bundled: true }),
                None => on_path().map(|path| Lino { path, bundled: false }),
            }
        })
        .as_ref()
    }

    /// Whether this is the lino that ships with xharness.
    pub fn bundled(&self) -> bool {
        self.bundled
    }

    /// The folder the bundled lino is in, for terminals' `PATH`.
    pub fn bundled_dir() -> Option<&'static Path> {
        Self::get().filter(|lino| lino.bundled).and_then(|lino| lino.path.parent())
    }

    /// Runs `lino <args> --json` in `folder` and reads its reply.
    fn call<T: DeserializeOwned>(&self, folder: &Path, args: &[&str]) -> Result<Reply<T>, String> {
        let output = crate::services::command(&self.path)
            .args(args)
            .arg("--json")
            .current_dir(folder)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| format!("can't run lino: {error}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        // One JSON object; anything else printed before it is a note.
        serde_json::from_str(&stdout)
            .or_else(|_| {
                let line = stdout.lines().rev().find(|line| line.trim_start().starts_with('{'));
                serde_json::from_str(line.unwrap_or_default())
            })
            .map_err(|_| {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let said = stderr.trim().lines().last().unwrap_or("no reply");
                format!("lino failed: {said}")
            })
    }

    /// Runs a command that only reports success or failure.
    fn act(&self, folder: &Path, args: &[&str]) -> Result<(), String> {
        let reply = self.call::<serde_json::Value>(folder, args)?;
        if reply.ok { Ok(()) } else { Err(reply.message) }
    }

    /// What lino is doing in `folder`. Never starts it.
    pub fn snapshot(&self, folder: &Path) -> Snapshot {
        if !is_enabled(folder) {
            return Snapshot::NotEnabled;
        }
        // `status` and `stats` answer from the live process, or say there's
        // none; unlike file commands, they don't start one.
        match self.call::<Status>(folder, &["status"]) {
            Err(error) => Snapshot::Failed(error),
            Ok(reply) if reply.outcome == "not_running" => Snapshot::Stopped,
            Ok(reply) if !reply.ok => Snapshot::Failed(reply.message),
            Ok(reply) => Snapshot::Running {
                status: reply.data.unwrap_or_default(),
                stats: self.call::<Stats>(folder, &["stats"]).ok().and_then(|reply| reply.data),
            },
        }
    }

    /// The `count` most recent changes, newest first.
    pub fn history(&self, folder: &Path, count: usize) -> Result<Vec<Change>, String> {
        let reply = self.call::<HistoryData>(folder, &["history", "-k", &count.to_string()])?;
        match reply.data {
            Some(data) => Ok(data.changes),
            None if reply.ok || reply.outcome == "empty" => Ok(Vec::new()),
            None => Err(reply.message),
        }
    }

    /// Sets lino up in `folder` (`.lino/`, plus a `.linoignore` with
    /// defaults) and starts its live process. Fine to repeat.
    pub fn enable(&self, folder: &Path) -> Result<(), String> {
        let dir = folder.to_string_lossy();
        self.act(folder, &["init", &dir])?;
        self.start(folder)
    }

    pub fn start(&self, folder: &Path) -> Result<(), String> {
        self.act(folder, &["run", &folder.to_string_lossy()])
    }

    pub fn stop(&self, folder: &Path) -> Result<(), String> {
        match self.call::<serde_json::Value>(folder, &["stop"])? {
            reply if reply.ok || reply.outcome == "not_running" => Ok(()),
            reply => Err(reply.message),
        }
    }

    /// Re-reads the whole folder into the index.
    pub fn reindex(&self, folder: &Path) -> Result<(), String> {
        self.act(folder, &["index"])
    }
}

// Routed: what the app asks of lino, run in the server ----------------------------

/// Whether there's a lino where the work runs: `Some(true)` for the bundled
/// one, `Some(false)` for one on PATH, `None` for none.
pub fn available() -> Option<bool> {
    route("lino.available", (), || Lino::get().map(Lino::bundled))
}

/// What lino is doing in `folder` (`None`: no lino).
pub fn snapshot(folder: &Path) -> Option<Snapshot> {
    route("lino.snapshot", (folder,), || Lino::get().map(|lino| lino.snapshot(folder)))
}

pub fn history(folder: &Path, count: usize) -> Result<Vec<Change>, String> {
    route("lino.history", (folder, count), || installed()?.history(folder, count))
}

pub fn enable(folder: &Path) -> Result<(), String> {
    route("lino.enable", (folder,), || installed()?.enable(folder))
}

pub fn start(folder: &Path) -> Result<(), String> {
    route("lino.start", (folder,), || installed()?.start(folder))
}

pub fn stop(folder: &Path) -> Result<(), String> {
    route("lino.stop", (folder,), || installed()?.stop(folder))
}

pub fn reindex(folder: &Path) -> Result<(), String> {
    route("lino.reindex", (folder,), || installed()?.reindex(folder))
}

fn installed() -> Result<&'static Lino, String> {
    Lino::get().ok_or_else(|| "lino isn't installed".to_string())
}

/// The server runs these.
pub(crate) fn dispatch(method: &str, params: Value) -> Option<Result<Value, String>> {
    Some(match method {
        "lino.available" => handle_call(params, |(): ()| available()),
        "lino.snapshot" => handle_call(params, |(folder,): (PathBuf,)| snapshot(&folder)),
        "lino.history" => handle_call(params, |(folder, count): (PathBuf, usize)| history(&folder, count)),
        "lino.enable" => handle_call(params, |(folder,): (PathBuf,)| enable(&folder)),
        "lino.start" => handle_call(params, |(folder,): (PathBuf,)| start(&folder)),
        "lino.stop" => handle_call(params, |(folder,): (PathBuf,)| stop(&folder)),
        "lino.reindex" => handle_call(params, |(folder,): (PathBuf,)| reindex(&folder)),
        _ => return None,
    })
}

/// Whether lino is set up in `folder`.
pub fn is_enabled(folder: &Path) -> bool {
    folder.join(LINO_DIR).is_dir()
}

/// Writes the bundle to `dir`, unless it's already there, and returns the
/// path of `lino`. `lino-core` goes first, so a new `lino` never runs an
/// old core; each file is renamed into place, so a running lino is never
/// overwritten underneath itself.
#[cfg_attr(not(lino_bundled), allow(dead_code))]
fn install(dir: &Path, bundle: &Bundle) -> io::Result<PathBuf> {

    fs::create_dir_all(dir)?;
    for (name, bytes) in [("lino-core", bundle.core), ("lino", bundle.lino)] {
        let path = dir.join(name);
        if fs::metadata(&path).is_ok_and(|meta| meta.len() == bytes.len() as u64) {
            continue;
        }
        let temp = dir.join(format!(".{name}.{}", std::process::id()));
        fs::write(&temp, bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temp, fs::Permissions::from_mode(0o755))?;
        }
        fs::rename(&temp, &path)?;
    }
    Ok(dir.join("lino"))
}

/// `lino` on `PATH`, if any.
fn on_path() -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|dir| dir.join(if cfg!(windows) { "lino.exe" } else { "lino" }))
        .find(|path| path.is_file())
}

// Replies ------------------------------------------------------------------------

/// lino's JSON envelope: `{"version":1,"ok":…,"outcome":…,"data":…}`.
#[derive(Deserialize)]
struct Reply<T> {
    ok: bool,
    #[serde(default)]
    outcome: String,
    data: Option<T>,
    #[serde(default)]
    message: String,
}

/// What lino is doing in a folder.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Snapshot {
    /// The folder has no `.lino`.
    NotEnabled,
    /// Set up, with no live process (it stops after a while idle).
    Stopped,
    Running { status: Status, stats: Option<Stats> },
    Failed(String),
}

/// `lino status`: the index and the file watcher.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Status {
    pub id: String,
    pub pid: u32,
    pub uptime_s: u64,
    pub indexed: bool,
    pub totals: Totals,
    pub watcher: String,
    /// Changed files waiting to be indexed.
    pub pending: u64,
    pub last_reconcile: Option<Reconcile>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Totals {
    pub files: u64,
    pub lines: u64,
    pub binary: u64,
}

/// The last time lino compared the index with the folder.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Reconcile {
    pub at: String,
    pub duration_ms: u64,
    pub files: u64,
    pub added: u64,
    pub modified: u64,
    pub removed: u64,
    pub moved: u64,
}

/// `lino stats`: sizes, memory, and how calls went.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Stats {
    pub process: ProcessStats,
    pub index: IndexStats,
    pub history: HistoryStats,
    /// Files changed outside lino (an editor, git) that it picked up.
    pub external: u64,
    pub calls: u64,
    pub rates: Vec<Rate>,
    pub reindex: Latency,
    pub commands: Vec<CommandStats>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProcessStats {
    pub peak_rss_bytes: u64,
    pub heap_bytes: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct IndexStats {
    pub files: u64,
    pub lines: u64,
    pub db_bytes: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HistoryStats {
    pub changes: u64,
    pub db_bytes: u64,
}

/// How often edits failed a check (`conflict`: the file changed since it
/// was read; `anchor_mismatch`: a line anchor didn't match).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Rate {
    pub outcome: String,
    pub count: u64,
    pub of: u64,
    pub rate: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Latency {
    pub count: u64,
    pub p50_us: u64,
    pub p95_us: u64,
    pub max_us: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CommandStats {
    pub name: String,
    pub count: u64,
    pub p50_us: u64,
    pub p95_us: u64,
    pub max_us: u64,
    pub outcomes: BTreeMap<String, u64>,
}

#[derive(Deserialize)]
struct HistoryData {
    #[serde(default)]
    changes: Vec<Change>,
}

/// A change in lino's history.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Change {
    pub id: u64,
    pub time: String,
    /// Who made it, if they said (`--by`).
    pub author: Option<String>,
    pub op: String,
    pub path: String,
    pub summary: String,
}

// Formatting -----------------------------------------------------------------------

/// `98311` → `98,311`.
pub fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (ix, digit) in digits.chars().enumerate() {
        if ix > 0 && (digits.len() - ix) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// A latency: `971 µs`, `12.4 ms`, `1.2 s`.
pub fn format_micros(us: u64) -> String {
    match us {
        0..1_000 => format!("{us} µs"),
        1_000..100_000 => format!("{:.1} ms", us as f64 / 1e3),
        100_000..1_000_000 => format!("{} ms", us / 1_000),
        _ => format!("{:.1} s", us as f64 / 1e6),
    }
}

/// How long something has been going: `45s`, `12m`, `2h 5m`, `3d 4h`.
pub fn format_span(secs: u64) -> String {
    let (m, h, d) = (secs / 60, secs / 3600, secs / 86400);
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{m}m"),
        3600..86400 => format!("{h}h {}m", m % 60),
        _ => format!("{d}d {}h", h % 24),
    }
}

/// `at` (RFC 3339) relative to `now` (Unix seconds): `just now`, `5m ago`.
pub fn ago(at: &str, now: i64) -> Option<String> {
    let secs = (now - parse_time(at)?).max(0) as u64;
    Some(if secs < 5 { "just now".to_string() } else { format!("{} ago", format_span(secs).split(' ').next()?) })
}

/// Unix seconds of an RFC 3339 time, e.g. `2026-11-01T21:08:34.38118+03:00`.
pub fn parse_time(text: &str) -> Option<i64> {
    let (date, rest) = text.split_once(['T', 't', ' '])?;
    let mut date = date.splitn(3, '-').map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (date.next()??, date.next()??, date.next()??);
    let zone_at = rest.find(['Z', 'z', '+', '-'])?;
    let (time, zone) = rest.split_at(zone_at);
    let mut time = time.splitn(3, ':');
    let hours: i64 = time.next()?.parse().ok()?;
    let minutes: i64 = time.next()?.parse().ok()?;
    let seconds: f64 = time.next()?.parse().ok()?;
    let offset = if zone.eq_ignore_ascii_case("z") {
        0
    } else {
        let sign = if zone.starts_with('-') { -1 } else { 1 };
        let (h, m) = zone[1..].split_once(':')?;
        sign * (h.parse::<i64>().ok()? * 3600 + m.parse::<i64>().ok()? * 60)
    };
    let days = days_from_civil(year, month, day);
    Some(days * 86400 + hours * 3600 + minutes * 60 + seconds as i64 - offset)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_parse_with_their_zone() {
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_time("2026-11-01T21:08:34.38118+03:00"), Some(1_793_556_514));
        assert_eq!(parse_time("2000-02-29T23:59:59-05:30"), Some(951_888_599));
        assert_eq!(parse_time("yesterday"), None);
        assert_eq!(ago("2026-11-01T21:08:34+03:00", 1_793_556_514 + 300).as_deref(), Some("5m ago"));
        assert_eq!(ago("2026-11-01T21:08:34+03:00", 1_793_556_514 + 2).as_deref(), Some("just now"));
    }

    #[test]
    fn numbers_read_well() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(98_311), "98,311");
        assert_eq!(group_digits(1_234_567), "1,234,567");
        assert_eq!(format_micros(971), "971 µs");
        assert_eq!(format_micros(12_400), "12.4 ms");
        assert_eq!(format_micros(250_000), "250 ms");
        assert_eq!(format_micros(1_200_000), "1.2 s");
        assert_eq!(format_span(45), "45s");
        assert_eq!(format_span(7_500), "2h 5m");
        assert_eq!(format_span(273_600), "3d 4h");
    }

    #[test]
    fn replies_parse_from_lino_json() {
        let json = r#"{"version":1,"ok":true,"outcome":"ok","data":{"root":"/p","id":"g4c6y7","live":true,
            "pid":64901,"uptime_s":3,"indexed":true,"totals":{"files":2,"lines":25,"binary":0},
            "watcher":"kqueue, 1 dirs","pending":0,"last_reconcile":{"at":"2026-11-01T20:59:17+03:00",
            "duration_ms":4,"files":2,"added":1,"modified":0,"removed":0,"moved":0}}}"#;
        let reply: Reply<Status> = serde_json::from_str(json).unwrap();
        let status = reply.data.unwrap();
        assert_eq!((status.id.as_str(), status.totals.lines), ("g4c6y7", 25));
        assert_eq!(status.last_reconcile.unwrap().added, 1);

        let json = r#"{"version":1,"ok":false,"outcome":"not_running","message":"no live lino process"}"#;
        let reply: Reply<Status> = serde_json::from_str(json).unwrap();
        assert!(!reply.ok && reply.outcome == "not_running" && reply.data.is_none());
    }

    /// The bundled lino unpacks, sets a folder up, runs, and stops.
    #[test]
    fn the_bundled_lino_runs_in_a_folder() {
        let Some(bundle) = BUNDLE.as_ref() else {
            return;
        };
        let root = std::env::temp_dir().join(format!("xh-lino-{}", std::process::id()));
        fs::remove_dir_all(&root).ok();
        let (bin, folder) = (root.join("bin"), root.join("project"));
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("main.rs"), "fn main() {\n    println!(\"hi\");\n}\n").unwrap();
        // macOS resolves the temp dir through /private; lino reports real paths.
        let folder = folder.canonicalize().unwrap();

        let lino = Lino { path: install(&bin, bundle).unwrap(), bundled: true };
        assert!(bin.join("lino-core").is_file());
        install(&bin, bundle).expect("installing again is a no-op");

        assert!(matches!(lino.snapshot(&folder), Snapshot::NotEnabled));
        lino.enable(&folder).unwrap();
        match lino.snapshot(&folder) {
            Snapshot::Running { status, stats } => {
                assert_eq!(status.totals.files, 2, "main.rs and .linoignore");
                assert!(stats.is_some());
            }
            other => panic!("expected a live process, got {other:?}"),
        }
        assert!(lino.history(&folder, 5).is_ok());
        lino.stop(&folder).unwrap();
        assert!(matches!(lino.snapshot(&folder), Snapshot::Stopped));
        lino.stop(&folder).expect("stopping twice is fine");
        fs::remove_dir_all(&root).ok();
    }
}

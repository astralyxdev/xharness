//! SFTP: files on other machines, for SFTP windows (see `windows/sftp`).
//!
//! Connections are made by the server, like shells: a remote project's
//! transfers go straight between its machine and the SFTP host, and they
//! keep going when the app quits. The app keeps the list of sites and
//! sends the one to connect to with [`connect`]; the server keeps the
//! connection, under an id the window uses for everything after.
//!
//! Hosts are checked against `~/.ssh/known_hosts` (the server machine's),
//! as `ssh` does. A host that isn't there yet comes back as
//! [`Connected::UnknownHost`] with its key's fingerprint; connecting again
//! with that fingerprint trusts it and adds it to `known_hosts`. A host
//! whose key changed is refused.
//!
//! SSH here is pure Rust (russh, with RustCrypto): the server cross-builds
//! for Linux without a C compiler (see `scripts/build/server.sh`).

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, UNIX_EPOCH},
};

use russh::client::{self, Handle};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKey};
use russh_sftp::client::SftpSession;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::services::rpc::{Unreachable, handle_call, route};

/// How long connecting (and signing in) may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// A connection nobody has used for this long, with nothing transferring,
/// is closed (the app may have quit without disconnecting).
const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Bytes copied at a time in a transfer.
const CHUNK: usize = 256 << 10;

// What the app sends and gets back -------------------------------------------------

/// Where to connect, as a site in the app's list has it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Site {
    pub host: String,
    /// 22 when 0.
    pub port: u16,
    pub user: String,
    pub auth: Auth,
}

impl Site {
    pub fn port(&self) -> u16 {
        if self.port == 0 { 22 } else { self.port }
    }

    /// `user@host` (and `:port` when it isn't 22).
    pub fn address(&self) -> String {
        let host = if self.port() == 22 { self.host.clone() } else { format!("{}:{}", self.host, self.port()) };
        if self.user.is_empty() { host } else { format!("{}@{host}", self.user) }
    }
}

/// How to sign in.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Auth {
    /// The SSH agent's keys, then the usual key files in `~/.ssh`
    /// (`id_ed25519`, `id_ecdsa`, `id_rsa`): the connecting machine's, so a
    /// remote project uses its server's.
    #[default]
    Agent,
    Password {
        #[serde(default)]
        password: String,
    },
    /// A private key file (on the connecting machine; `~` is home).
    KeyFile {
        #[serde(default)]
        path: PathBuf,
        /// For an encrypted key; empty if it isn't.
        #[serde(default)]
        passphrase: String,
    },
}

/// What [`connect`] did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Connected {
    /// Connected and signed in: `session` names the connection from now on;
    /// `home` is the folder the server starts in.
    Ok { session: String, home: String },
    /// The host isn't in `known_hosts`. Connect again with `fingerprint` to
    /// trust it.
    UnknownHost { fingerprint: String },
    /// The host's key isn't the one in `known_hosts`: someone may be in the
    /// middle, or the host was reinstalled. Not connected.
    ChangedHost { fingerprint: String },
    Failed(String),
}

impl Unreachable for Connected {
    fn unreachable(error: String) -> Self {
        Connected::Failed(error)
    }
}

/// Why a call on a connection didn't work.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Failure {
    /// The connection is gone (closed, timed out, or the server started
    /// again): connect again.
    Disconnected,
    Failed(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Disconnected => f.write_str("the connection was closed"),
            Failure::Failed(error) => f.write_str(error),
        }
    }
}

impl<T> Unreachable for Result<T, Failure> {
    fn unreachable(error: String) -> Self {
        Err(Failure::Failed(error))
    }
}

/// A file or folder on the SFTP host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteEntry {
    pub name: String,
    /// Absolute, with `/`s.
    pub path: String,
    /// A folder, or a link to one.
    pub is_dir: bool,
    pub is_link: bool,
    pub size: u64,
    /// Seconds since the epoch.
    pub modified: Option<u64>,
    /// Unix mode bits (`0o755`…), when the host says.
    pub permissions: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Upload,
    Download,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferState {
    /// Finding what to copy (walking folders).
    Preparing,
    Running,
    Done,
    Failed(String),
    Cancelled,
}

impl TransferState {
    pub fn finished(&self) -> bool {
        !matches!(self, TransferState::Preparing | TransferState::Running)
    }
}

/// A copy to or from the host: one or more files and folders, each with
/// everything in it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transfer {
    pub id: u64,
    pub direction: Direction,
    /// What was asked for: the item's name, or the first one's.
    pub name: String,
    /// How many items were asked for (folders count once).
    pub items: usize,
    /// The folder it goes into (remote for uploads, local for downloads).
    pub into: String,
    /// The file being copied now.
    pub current: String,
    pub files_done: usize,
    pub files_total: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub state: TransferState,
}

// The app's side --------------------------------------------------------------------

/// Connects to `site` and signs in. `trust` is a fingerprint the user
/// agreed to (see [`Connected::UnknownHost`]).
pub fn connect(site: &Site, trust: Option<&str>) -> Connected {
    route("sftp.connect", (site, trust), || connect_here(site, trust))
}

/// What's in `path`, unsorted. `.` and `..` are left out.
pub fn list(session: &str, path: &str) -> Result<Vec<RemoteEntry>, Failure> {
    route("sftp.list", (session, path), || list_here(session, path))
}

/// `path` made absolute and without `.`/`..` by the host. Fails if it
/// isn't there.
pub fn canonicalize(session: &str, path: &str) -> Result<String, Failure> {
    route("sftp.canonicalize", (session, path), || {
        with_connection(session, async |sftp| sftp.canonicalize(path).await.map_err(fail))
    })
}

pub fn create_dir(session: &str, path: &str) -> Result<(), Failure> {
    route("sftp.create_dir", (session, path), || {
        with_connection(session, async |sftp| sftp.create_dir(path).await.map_err(fail))
    })
}

pub fn rename(session: &str, from: &str, to: &str) -> Result<(), Failure> {
    route("sftp.rename", (session, from, to), || {
        with_connection(session, async |sftp| sftp.rename(from, to).await.map_err(fail))
    })
}

/// Deletes `paths`, folders with everything in them.
pub fn remove(session: &str, paths: &[String]) -> Result<(), Failure> {
    route("sftp.remove", (session, paths), || {
        with_connection(session, async |sftp| {
            for path in paths {
                remove_tree(sftp, path.clone()).await.map_err(fail)?;
            }
            Ok(())
        })
    })
}

/// Starts copying `sources` (on the connecting machine) into the host's
/// folder `into`. Files there with the same names are replaced.
pub fn upload(session: &str, sources: &[PathBuf], into: &str) -> Result<u64, Failure> {
    route("sftp.upload", (session, sources, into), || upload_here(session, sources, into))
}

/// Starts copying the host's `sources` into the local folder `into`.
/// Files there with the same names are replaced.
pub fn download(session: &str, sources: &[String], into: &Path) -> Result<u64, Failure> {
    route("sftp.download", (session, sources, into), || download_here(session, sources, into))
}

/// The connection's transfers, oldest first, finished ones included until
/// [`clear_finished`].
pub fn transfers(session: &str) -> Vec<Transfer> {
    route("sftp.transfers", (session,), || transfers_here(session))
}

pub fn cancel(transfer: u64) -> Option<()> {
    route("sftp.cancel", (transfer,), || {
        let jobs = JOBS.lock().unwrap();
        jobs.get(&transfer).map(|job| job.cancel.store(true, Ordering::SeqCst))
    })
}

pub fn clear_finished(session: &str) -> Option<()> {
    route("sftp.clear_finished", (session,), || {
        JOBS.lock().unwrap().retain(|_, job| job.session != session || !job.status.lock().unwrap().state.finished());
        Some(())
    })
}

/// Closes the connection, stopping its transfers.
pub fn disconnect(session: &str) -> Option<()> {
    route("sftp.disconnect", (session,), || {
        disconnect_here(session);
        Some(())
    })
}

/// Runs a call the app sent (see `rpc.rs`).
pub(crate) fn dispatch(method: &str, params: Value) -> Option<Result<Value, String>> {
    Some(match method {
        "sftp.connect" => handle_call(params, |(site, trust): (Site, Option<String>)| connect_here(&site, trust.as_deref())),
        "sftp.list" => handle_call(params, |(session, path): (String, String)| list_here(&session, &path)),
        "sftp.canonicalize" => handle_call(params, |(session, path): (String, String)| canonicalize(&session, &path)),
        "sftp.create_dir" => handle_call(params, |(session, path): (String, String)| create_dir(&session, &path)),
        "sftp.rename" => handle_call(params, |(session, from, to): (String, String, String)| rename(&session, &from, &to)),
        "sftp.remove" => handle_call(params, |(session, paths): (String, Vec<String>)| remove(&session, &paths)),
        "sftp.upload" => handle_call(params, |(session, sources, into): (String, Vec<PathBuf>, String)| {
            upload_here(&session, &sources, &into)
        }),
        "sftp.download" => handle_call(params, |(session, sources, into): (String, Vec<String>, PathBuf)| {
            download_here(&session, &sources, &into)
        }),
        "sftp.transfers" => handle_call(params, |(session,): (String,)| transfers_here(&session)),
        "sftp.cancel" => handle_call(params, |(transfer,): (u64,)| cancel(transfer)),
        "sftp.clear_finished" => handle_call(params, |(session,): (String,)| clear_finished(&session)),
        "sftp.disconnect" => handle_call(params, |(session,): (String,)| disconnect(&session)),
        _ => return None,
    })
}

/// `name` in the host's folder `dir`.
pub fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') { format!("{dir}{name}") } else { format!("{dir}/{name}") }
}

/// The folder `path` is in (`/` for `/` itself).
pub fn parent(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(at) => trimmed[..at].to_string(),
    }
}

/// The last part of `path`.
pub fn file_name(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or(trimmed)
}

// The server's side -----------------------------------------------------------------

struct Connection {
    handle: Handle<Client>,
    sftp: SftpSession,
    last_used: Mutex<Instant>,
}

impl Connection {
    fn closed(&self) -> bool {
        self.handle.is_closed()
    }
}

struct Job {
    session: String,
    status: Mutex<Transfer>,
    cancel: AtomicBool,
}

static CONNECTIONS: Mutex<Option<HashMap<String, Arc<Connection>>>> = Mutex::new(None);
static JOBS: Mutex<std::collections::BTreeMap<u64, Arc<Job>>> = Mutex::new(std::collections::BTreeMap::new());
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// SSH runs on tokio; the server's calls come on threads of their own and
/// wait here.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("sftp")
            .enable_all()
            .build()
            .expect("the SFTP runtime starts");
        runtime.spawn(close_idle());
        runtime
    })
}

/// Closes connections nobody uses (see [`IDLE_TIMEOUT`]).
async fn close_idle() {
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        let idle: Vec<String> = {
            let connections = CONNECTIONS.lock().unwrap();
            let jobs = JOBS.lock().unwrap();
            let busy = |session: &str| {
                jobs.values()
                    .any(|job| job.session == session && !job.status.lock().unwrap().state.finished())
            };
            connections
                .iter()
                .flatten()
                .filter(|(session, connection)| {
                    connection.closed()
                        || (connection.last_used.lock().unwrap().elapsed() > IDLE_TIMEOUT && !busy(session))
                })
                .map(|(session, _)| session.clone())
                .collect()
        };
        for session in idle {
            disconnect_here(&session);
        }
    }
}

fn connection(session: &str) -> Result<Arc<Connection>, Failure> {
    let mut connections = CONNECTIONS.lock().unwrap();
    let map = connections.get_or_insert_with(HashMap::new);
    match map.get(session) {
        Some(connection) if connection.closed() => {
            map.remove(session);
            Err(Failure::Disconnected)
        }
        Some(connection) => {
            *connection.last_used.lock().unwrap() = Instant::now();
            Ok(connection.clone())
        }
        None => Err(Failure::Disconnected),
    }
}

/// Runs `f` on the session's SFTP connection, waiting for it. A failure
/// on a connection that has since closed is [`Failure::Disconnected`].
fn with_connection<R>(
    session: &str,
    f: impl AsyncFnOnce(&SftpSession) -> Result<R, Failure>,
) -> Result<R, Failure> {
    let connection = connection(session)?;
    let result = runtime().block_on(f(&connection.sftp));
    match result {
        Err(_) if connection.closed() => {
            disconnect_here(session);
            Err(Failure::Disconnected)
        }
        result => result,
    }
}

fn fail(error: impl std::fmt::Display) -> Failure {
    Failure::Failed(error.to_string())
}

/// Tests check hosts against a file of their own, not `~/.ssh/known_hosts`.
static KNOWN_HOSTS: Mutex<Option<PathBuf>> = Mutex::new(None);

fn known_hosts_path() -> Option<PathBuf> {
    KNOWN_HOSTS.lock().unwrap().clone()
}

/// What the host check found, for [`connect_here`] to report.
#[derive(Clone, Debug)]
enum HostCheck {
    Unknown(String),
    Changed(String),
}

struct Client {
    host: String,
    port: u16,
    trust: Option<String>,
    check: Arc<Mutex<Option<HostCheck>>>,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKey) -> Result<bool, Self::Error> {
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        let known_hosts = known_hosts_path();
        let check = match &known_hosts {
            Some(path) => russh::keys::check_known_hosts_path(&self.host, self.port, key, path),
            None => russh::keys::check_known_hosts(&self.host, self.port, key),
        };
        match check {
            Ok(true) => Ok(true),
            Err(russh::keys::Error::KeyChanged { .. }) => {
                *self.check.lock().unwrap() = Some(HostCheck::Changed(fingerprint));
                Ok(false)
            }
            // Not there yet (or no known_hosts at all).
            _ if self.trust.as_deref() == Some(fingerprint.as_str()) => {
                match &known_hosts {
                    Some(path) => russh::keys::known_hosts::learn_known_hosts_path(&self.host, self.port, key, path).ok(),
                    None => russh::keys::known_hosts::learn_known_hosts(&self.host, self.port, key).ok(),
                };
                Ok(true)
            }
            _ => {
                *self.check.lock().unwrap() = Some(HostCheck::Unknown(fingerprint));
                Ok(false)
            }
        }
    }
}

fn connect_here(site: &Site, trust: Option<&str>) -> Connected {
    if site.host.trim().is_empty() {
        return Connected::Failed("No host to connect to.".into());
    }
    if site.user.trim().is_empty() {
        return Connected::Failed("No user name to sign in with.".into());
    }
    runtime().block_on(async {
        match tokio::time::timeout(CONNECT_TIMEOUT, open(site, trust)).await {
            Ok(connected) => connected,
            Err(_) => Connected::Failed(format!("{} didn't answer in time.", site.host)),
        }
    })
}

async fn open(site: &Site, trust: Option<&str>) -> Connected {
    let host = site.host.trim().to_string();
    let port = site.port();
    let check = Arc::new(Mutex::new(None));
    let client = Client {
        host: host.clone(),
        port,
        trust: trust.map(str::to_string),
        check: check.clone(),
    };
    let config = Arc::new(client::Config {
        // Finds a connection that died (a laptop that slept, a dropped
        // network) instead of waiting on it for ever.
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        ..Default::default()
    });
    let mut handle = match client::connect(config, (host.as_str(), port), client).await {
        Ok(handle) => handle,
        Err(error) => {
            return match check.lock().unwrap().take() {
                Some(HostCheck::Unknown(fingerprint)) => Connected::UnknownHost { fingerprint },
                Some(HostCheck::Changed(fingerprint)) => Connected::ChangedHost { fingerprint },
                None => Connected::Failed(describe_connect_error(&host, port, &error)),
            };
        }
    };
    if let Err(error) = authenticate(&mut handle, site).await {
        handle.disconnect(russh::Disconnect::ByApplication, "", "en").await.ok();
        return Connected::Failed(error);
    }
    let sftp = async {
        let channel = handle.channel_open_session().await.map_err(|e| e.to_string())?;
        channel.request_subsystem(true, "sftp").await.map_err(|e| e.to_string())?;
        let config = russh_sftp::client::Config {
            request_timeout_secs: 30,
            ..Default::default()
        };
        SftpSession::new_with_config(channel.into_stream(), config).await.map_err(|e| e.to_string())
    };
    let sftp = match sftp.await {
        Ok(sftp) => sftp,
        Err(error) => {
            handle.disconnect(russh::Disconnect::ByApplication, "", "en").await.ok();
            return Connected::Failed(format!("Signed in, but {host} has no SFTP: {error}"));
        }
    };
    let home = sftp.canonicalize(".").await.unwrap_or_else(|_| "/".to_string());
    let session = format!("sftp-{}-{}", std::process::id(), NEXT_ID.fetch_add(1, Ordering::SeqCst));
    let connection = Arc::new(Connection {
        handle,
        sftp,
        last_used: Mutex::new(Instant::now()),
    });
    CONNECTIONS.lock().unwrap().get_or_insert_with(HashMap::new).insert(session.clone(), connection);
    Connected::Ok { session, home }
}

fn describe_connect_error(host: &str, port: u16, error: &russh::Error) -> String {
    match error {
        russh::Error::IO(io) => match io.kind() {
            std::io::ErrorKind::ConnectionRefused => format!("{host} refused the connection on port {port}."),
            std::io::ErrorKind::TimedOut => format!("{host} didn't answer on port {port}."),
            _ => format!("Couldn't reach {host}: {io}"),
        },
        error => format!("Couldn't connect to {host}: {error}"),
    }
}

async fn authenticate(handle: &mut Handle<Client>, site: &Site) -> Result<(), String> {
    let user = site.user.trim().to_string();
    let host = site.host.trim();
    let rsa_hash = || async { handle.best_supported_rsa_hash().await.ok().flatten().flatten() };
    match &site.auth {
        Auth::Password { password } => {
            let accepted = handle.authenticate_password(&user, password).await.map_err(|e| e.to_string())?;
            if accepted.success() || keyboard_interactive(handle, &user, password).await {
                return Ok(());
            }
            Err(format!("{host} didn't accept the password for {user}."))
        }
        Auth::KeyFile { path, passphrase } => {
            let path = expand_home(path);
            let passphrase = (!passphrase.is_empty()).then_some(passphrase.as_str());
            let key = russh::keys::load_secret_key(&path, passphrase)
                .map_err(|error| format!("Couldn't use the key {}: {error}", path.display()))?;
            let hash = rsa_hash().await;
            let accepted = handle
                .authenticate_publickey(&user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                .await
                .map_err(|e| e.to_string())?;
            if accepted.success() {
                return Ok(());
            }
            Err(format!("{host} didn't accept the key {} for {user}.", path.display()))
        }
        Auth::Agent => {
            let hash = rsa_hash().await;
            if agent_sign_in(handle, &user, hash).await {
                return Ok(());
            }
            let ssh = std::env::home_dir().unwrap_or_default().join(".ssh");
            for name in ["id_ed25519", "id_ecdsa", "id_rsa"] {
                // Keys with a passphrase need it: those are for the agent
                // or the "Key file" sign-in.
                let Ok(key) = russh::keys::load_secret_key(ssh.join(name), None) else {
                    continue;
                };
                let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
                if handle.authenticate_publickey(&user, key).await.is_ok_and(|r| r.success()) {
                    return Ok(());
                }
            }
            Err(format!(
                "{host} didn't accept any key for {user}: none of the SSH agent's, nor ~/.ssh/id_ed25519, id_ecdsa or id_rsa. Add your key to the server, or sign in with a password."
            ))
        }
    }
}

/// Servers that turn plain passwords off often still take one through
/// keyboard-interactive (it asks; the answer is the password).
async fn keyboard_interactive(handle: &mut Handle<Client>, user: &str, password: &str) -> bool {
    use russh::client::KeyboardInteractiveAuthResponse as Response;
    let Ok(mut response) = handle.authenticate_keyboard_interactive_start(user, None).await else {
        return false;
    };
    for _ in 0..3 {
        match response {
            Response::Success => return true,
            Response::Failure { .. } => return false,
            Response::InfoRequest { prompts, .. } => {
                let answers = prompts.iter().map(|_| password.to_string()).collect();
                match handle.authenticate_keyboard_interactive_respond(answers).await {
                    Ok(next) => response = next,
                    Err(_) => return false,
                }
            }
        }
    }
    false
}

/// Tries each of the SSH agent's keys.
async fn agent_sign_in(handle: &mut Handle<Client>, user: &str, hash: Option<HashAlg>) -> bool {
    #[cfg(unix)]
    let agent = russh::keys::agent::client::AgentClient::connect_env().await;
    #[cfg(windows)]
    let agent = russh::keys::agent::client::AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent").await;
    let Ok(mut agent) = agent else {
        return false;
    };
    let keys = agent.request_identities().await.unwrap_or_default();
    for key in keys {
        if handle
            .authenticate_publickey_with(user, key, hash, &mut agent)
            .await
            .is_ok_and(|r| r.success())
        {
            return true;
        }
    }
    false
}

/// `~/…` with this machine's home.
fn expand_home(path: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => std::env::home_dir().unwrap_or_default().join(rest),
        Err(_) => path.to_path_buf(),
    }
}

fn list_here(session: &str, path: &str) -> Result<Vec<RemoteEntry>, Failure> {
    with_connection(session, async |sftp| {
        let dir = sftp.read_dir(path).await.map_err(fail)?;
        let mut entries = Vec::new();
        for entry in dir {
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            let meta = entry.metadata();
            let full = join(path, &name);
            let is_link = meta.is_symlink();
            // A link shows as what it points to (a broken one as a file).
            let target = if is_link { sftp.metadata(full.clone()).await.ok() } else { None };
            let shown = target.as_ref().unwrap_or(&meta);
            entries.push(RemoteEntry {
                name,
                path: full,
                is_dir: shown.is_dir(),
                is_link,
                size: shown.size.unwrap_or(0),
                modified: shown.mtime.map(u64::from),
                permissions: meta.permissions.map(|p| p & 0o7777),
            });
        }
        Ok(entries)
    })
}

type BoxFuture<'a, T> = Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

fn remove_tree(sftp: &SftpSession, path: String) -> BoxFuture<'_, Result<(), russh_sftp::client::error::Error>> {
    Box::pin(async move {
        let meta = sftp.symlink_metadata(path.clone()).await?;
        if meta.is_dir() {
            for entry in sftp.read_dir(path.clone()).await? {
                let name = entry.file_name();
                if name != "." && name != ".." {
                    remove_tree(sftp, join(&path, &name)).await?;
                }
            }
            sftp.remove_dir(path).await
        } else {
            sftp.remove_file(path).await
        }
    })
}

fn transfers_here(session: &str) -> Vec<Transfer> {
    JOBS.lock()
        .unwrap()
        .values()
        .filter(|job| job.session == session)
        .map(|job| job.status.lock().unwrap().clone())
        .collect()
}

fn disconnect_here(session: &str) {
    for job in JOBS.lock().unwrap().values().filter(|job| job.session == session) {
        job.cancel.store(true, Ordering::SeqCst);
    }
    let connection = CONNECTIONS.lock().unwrap().as_mut().and_then(|map| map.remove(session));
    if let Some(connection) = connection {
        runtime().spawn(async move {
            connection.sftp.close().await.ok();
            connection.handle.disconnect(russh::Disconnect::ByApplication, "", "en").await.ok();
        });
    }
}

/// One thing to do in a transfer.
enum Step {
    /// A folder to make (the destination's path).
    Folder(String),
    /// A file to copy: from, to, its size.
    File(String, String, u64),
}

fn new_job(session: &str, direction: Direction, names: Vec<String>, into: String) -> Arc<Job> {
    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    let job = Arc::new(Job {
        session: session.to_string(),
        status: Mutex::new(Transfer {
            id,
            direction,
            name: names.first().cloned().unwrap_or_default(),
            items: names.len(),
            into,
            current: String::new(),
            files_done: 0,
            files_total: 0,
            bytes_done: 0,
            bytes_total: 0,
            state: TransferState::Preparing,
        }),
        cancel: AtomicBool::new(false),
    });
    JOBS.lock().unwrap().insert(id, job.clone());
    job
}

impl Job {
    fn update(&self, f: impl FnOnce(&mut Transfer)) {
        f(&mut self.status.lock().unwrap());
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// Ends the transfer as `result` says (cancelled wins).
    fn finish(&self, result: Result<(), String>) {
        let cancelled = self.cancelled();
        self.update(|status| {
            status.current.clear();
            status.state = match result {
                _ if cancelled => TransferState::Cancelled,
                Ok(()) => TransferState::Done,
                Err(error) => TransferState::Failed(error),
            };
        });
    }

    fn plan(&self, steps: &[Step]) {
        let (files, bytes) = steps.iter().fold((0, 0), |(files, bytes), step| match step {
            Step::File(_, _, size) => (files + 1, bytes + size),
            Step::Folder(_) => (files, bytes),
        });
        self.update(|status| {
            status.files_total = files;
            status.bytes_total = bytes;
            status.state = TransferState::Running;
        });
    }
}

fn upload_here(session: &str, sources: &[PathBuf], into: &str) -> Result<u64, Failure> {
    let connection = connection(session)?;
    let names = sources
        .iter()
        .map(|path| path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
        .collect();
    let job = new_job(session, Direction::Upload, names, into.to_string());
    let id = job.status.lock().unwrap().id;
    let (sources, into) = (sources.to_vec(), into.to_string());
    runtime().spawn(async move {
        let steps = tokio::task::spawn_blocking(move || local_steps(&sources, &into)).await;
        let result = match steps {
            Ok(Ok(steps)) => {
                job.plan(&steps);
                run_upload(&connection.sftp, &job, &steps).await
            }
            Ok(Err(error)) => Err(error),
            Err(error) => Err(error.to_string()),
        };
        job.finish(result);
    });
    Ok(id)
}

/// The folders to make and files to copy for an upload, parents first.
fn local_steps(sources: &[PathBuf], into: &str) -> Result<Vec<Step>, String> {
    fn walk(path: &Path, remote: String, steps: &mut Vec<Step>) -> Result<(), String> {
        let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if meta.is_dir() {
            steps.push(Step::Folder(remote.clone()));
            let mut children: Vec<_> = std::fs::read_dir(path)
                .map_err(|e| format!("{}: {e}", path.display()))?
                .filter_map(Result::ok)
                .collect();
            children.sort_by_key(|entry| entry.file_name());
            for child in children {
                let name = child.file_name().to_string_lossy().into_owned();
                walk(&child.path(), join(&remote, &name), steps)?;
            }
        } else if meta.is_file() {
            steps.push(Step::File(path.to_string_lossy().into_owned(), remote, meta.len()));
        } else if meta.file_type().is_symlink() {
            // A link to a file goes as that file; links to folders are
            // left out (they can loop).
            if let Ok(target) = std::fs::metadata(path)
                && target.is_file()
            {
                steps.push(Step::File(path.to_string_lossy().into_owned(), remote, target.len()));
            }
        }
        Ok(())
    }
    let mut steps = Vec::new();
    for source in sources {
        let name = source.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        walk(source, join(into, &name), &mut steps)?;
    }
    Ok(steps)
}

async fn run_upload(sftp: &SftpSession, job: &Job, steps: &[Step]) -> Result<(), String> {
    let mut buffer = vec![0u8; CHUNK];
    for step in steps {
        if job.cancelled() {
            return Ok(());
        }
        match step {
            Step::Folder(remote) => {
                // Already there is fine.
                if !sftp.metadata(remote.clone()).await.is_ok_and(|m| m.is_dir()) {
                    sftp.create_dir(remote.clone()).await.map_err(|e| format!("{remote}: {e}"))?;
                }
            }
            Step::File(local, remote, _) => {
                job.update(|status| status.current = file_name(remote).to_string());
                let mut from = tokio::fs::File::open(local).await.map_err(|e| format!("{local}: {e}"))?;
                let mut to = sftp.create(remote.clone()).await.map_err(|e| format!("{remote}: {e}"))?;
                loop {
                    if job.cancelled() {
                        break;
                    }
                    let read = from.read(&mut buffer).await.map_err(|e| format!("{local}: {e}"))?;
                    if read == 0 {
                        break;
                    }
                    to.write_all(&buffer[..read]).await.map_err(|e| format!("{remote}: {e}"))?;
                    job.update(|status| status.bytes_done += read as u64);
                }
                to.shutdown().await.map_err(|e| format!("{remote}: {e}"))?;
                // Keep its time, as FileZilla does.
                if let Ok(modified) = tokio::fs::metadata(local).await.and_then(|m| m.modified()) {
                    let mut attrs = russh_sftp::protocol::FileAttributes::empty();
                    let secs = modified.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as u32;
                    attrs.mtime = Some(secs);
                    attrs.atime = Some(secs);
                    sftp.set_metadata(remote.clone(), attrs).await.ok();
                }
                if !job.cancelled() {
                    job.update(|status| status.files_done += 1);
                }
            }
        }
    }
    Ok(())
}

fn download_here(session: &str, sources: &[String], into: &Path) -> Result<u64, Failure> {
    let connection = connection(session)?;
    let names = sources.iter().map(|path| file_name(path).to_string()).collect();
    let job = new_job(session, Direction::Download, names, into.to_string_lossy().into_owned());
    let id = job.status.lock().unwrap().id;
    let (sources, into) = (sources.to_vec(), into.to_path_buf());
    runtime().spawn(async move {
        let result = async {
            let mut steps = Vec::new();
            for source in &sources {
                let local = into.join(file_name(source));
                remote_steps(&connection.sftp, source.clone(), local.to_string_lossy().into_owned(), &mut steps)
                    .await
                    .map_err(|e| format!("{source}: {e}"))?;
            }
            job.plan(&steps);
            run_download(&connection.sftp, &job, &steps).await
        }
        .await;
        job.finish(result);
    });
    Ok(id)
}

/// The folders to make and files to copy for a download, parents first.
fn remote_steps<'a>(
    sftp: &'a SftpSession,
    remote: String,
    local: String,
    steps: &'a mut Vec<Step>,
) -> BoxFuture<'a, Result<(), russh_sftp::client::error::Error>> {
    Box::pin(async move {
        // Links are followed: a link to a folder comes as the folder.
        let meta = sftp.metadata(remote.clone()).await?;
        if meta.is_dir() {
            steps.push(Step::Folder(local.clone()));
            let mut names: Vec<String> = sftp
                .read_dir(remote.clone())
                .await?
                .map(|entry| entry.file_name())
                .filter(|name| name != "." && name != "..")
                .collect();
            names.sort();
            for name in names {
                let child = Path::new(&local).join(&name).to_string_lossy().into_owned();
                remote_steps(sftp, join(&remote, &name), child, steps).await?;
            }
        } else {
            steps.push(Step::File(remote, local, meta.size.unwrap_or(0)));
        }
        Ok(())
    })
}

async fn run_download(sftp: &SftpSession, job: &Job, steps: &[Step]) -> Result<(), String> {
    let mut buffer = vec![0u8; CHUNK];
    for step in steps {
        if job.cancelled() {
            return Ok(());
        }
        match step {
            Step::Folder(local) => tokio::fs::create_dir_all(local).await.map_err(|e| format!("{local}: {e}"))?,
            Step::File(remote, local, _) => {
                job.update(|status| status.current = file_name(remote).to_string());
                let mut from = sftp.open(remote.clone()).await.map_err(|e| format!("{remote}: {e}"))?;
                let mut to = tokio::fs::File::create(local).await.map_err(|e| format!("{local}: {e}"))?;
                loop {
                    if job.cancelled() {
                        break;
                    }
                    let read = from.read(&mut buffer).await.map_err(|e| format!("{remote}: {e}"))?;
                    if read == 0 {
                        break;
                    }
                    to.write_all(&buffer[..read]).await.map_err(|e| format!("{local}: {e}"))?;
                    job.update(|status| status.bytes_done += read as u64);
                }
                to.flush().await.map_err(|e| format!("{local}: {e}"))?;
                let modified = from.metadata().await.ok().and_then(|m| m.mtime);
                drop(from);
                if job.cancelled() {
                    // Half a file isn't worth keeping.
                    drop(to);
                    tokio::fs::remove_file(local).await.ok();
                    continue;
                }
                if let Some(secs) = modified {
                    let time = UNIX_EPOCH + Duration::from_secs(u64::from(secs));
                    to.into_std().await.set_modified(time).ok();
                }
                job.update(|status| status.files_done += 1);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_paths_join_and_split() {
        assert_eq!(join("/home/me", "a.txt"), "/home/me/a.txt");
        assert_eq!(join("/", "etc"), "/etc");
        assert_eq!(parent("/home/me/a.txt"), "/home/me");
        assert_eq!(parent("/home"), "/");
        assert_eq!(parent("/"), "/");
        assert_eq!(parent("/home/me/"), "/home");
        assert_eq!(file_name("/home/me/a.txt"), "a.txt");
        assert_eq!(file_name("/home/me/"), "me");
    }

    #[test]
    fn sites_say_where_they_go() {
        let mut site = Site { host: "example.com".into(), user: "deploy".into(), ..Site::default() };
        assert_eq!((site.port(), site.address()), (22, "deploy@example.com".to_string()));
        site.port = 2222;
        assert_eq!(site.address(), "deploy@example.com:2222");
    }

    /// A real sshd (this machine's OpenSSH, run as you on a port of its
    /// own) and everything an SFTP window does with it: trusting the host,
    /// signing in with a key, listing, uploading and downloading folders,
    /// renaming and deleting. Skipped where there's no sshd.
    #[test]
    fn a_session_with_a_real_sshd() {
        let Some(server) = TestSshd::start() else {
            eprintln!("no sshd to test with: skipped");
            return;
        };
        *KNOWN_HOSTS.lock().unwrap() = Some(server.dir.join("known_hosts"));
        let site = Site {
            host: "127.0.0.1".into(),
            port: server.port,
            user: std::env::var("USER").unwrap_or_default(),
            auth: Auth::KeyFile { path: server.dir.join("client_key"), passphrase: String::new() },
        };

        // First time: the host is unknown. Trusting its key connects, and
        // remembers it.
        let Connected::UnknownHost { fingerprint } = connect(&site, None) else {
            panic!("an unknown host should be asked about");
        };
        assert!(fingerprint.starts_with("SHA256:"), "{fingerprint}");
        let Connected::Ok { session, .. } = connect(&site, Some(&fingerprint)) else {
            panic!("trusting the key should connect");
        };
        let Connected::Ok { session: again, .. } = connect(&site, None) else {
            panic!("a trusted host should connect without asking");
        };
        disconnect(&again);
        // A wrong key is refused.
        let wrong = Site { auth: Auth::KeyFile { path: server.dir.join("host_key"), passphrase: String::new() }, ..site.clone() };
        assert!(matches!(connect(&wrong, None), Connected::Failed(_)));

        // A folder with a file and a subfolder goes up…
        let local = server.dir.join("local");
        std::fs::create_dir_all(local.join("project/src")).unwrap();
        std::fs::write(local.join("project/readme.md"), "hello").unwrap();
        let big: Vec<u8> = (0..700_000u32).map(|n| (n % 251) as u8).collect();
        std::fs::write(local.join("project/src/data.bin"), &big).unwrap();
        let remote = server.dir.join("remote").to_string_lossy().into_owned();
        create_dir(&session, &remote).unwrap();
        let id = upload(&session, &[local.join("project")], &remote).unwrap();
        let done = wait(&session, id);
        assert_eq!(done.state, TransferState::Done, "{done:?}");
        assert_eq!((done.files_done, done.files_total, done.bytes_done), (2, 2, 700_005));

        let mut names: Vec<_> = list(&session, &join(&remote, "project")).unwrap().into_iter().map(|e| (e.name, e.is_dir)).collect();
        names.sort();
        assert_eq!(names, [("readme.md".to_string(), false), ("src".to_string(), true)]);

        // …and comes back down the same.
        let back = server.dir.join("back");
        std::fs::create_dir_all(&back).unwrap();
        let id = download(&session, &[join(&remote, "project")], &back).unwrap();
        assert_eq!(wait(&session, id).state, TransferState::Done);
        assert_eq!(std::fs::read(back.join("project/src/data.bin")).unwrap(), big);
        assert_eq!(std::fs::read_to_string(back.join("project/readme.md")).unwrap(), "hello");

        // Renaming, then deleting a folder with everything in it.
        let renamed = join(&remote, "renamed");
        rename(&session, &join(&remote, "project"), &renamed).unwrap();
        // (Resolved by the host: on a Mac, /var is /private/var.)
        assert!(canonicalize(&session, &renamed).unwrap().ends_with("/remote/renamed"));
        remove(&session, std::slice::from_ref(&renamed)).unwrap();
        assert!(list(&session, &remote).unwrap().is_empty());
        assert!(matches!(list(&session, &renamed), Err(Failure::Failed(_))));

        clear_finished(&session);
        assert!(transfers(&session).is_empty());
        disconnect(&session);
        assert_eq!(list(&session, &remote), Err(Failure::Disconnected));
    }

    fn wait(session: &str, id: u64) -> Transfer {
        let started = Instant::now();
        loop {
            let transfer = transfers(session).into_iter().find(|t| t.id == id).unwrap();
            if transfer.state.finished() || started.elapsed() > Duration::from_secs(30) {
                return transfer;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// sshd on 127.0.0.1, signing in only with `client_key`, in a folder
    /// of its own.
    struct TestSshd {
        dir: PathBuf,
        port: u16,
        child: std::process::Child,
    }

    impl TestSshd {
        fn start() -> Option<Self> {
            let sshd = ["/usr/sbin/sshd", "/usr/bin/sshd"].into_iter().map(Path::new).find(|p| p.exists())?;
            let sftp_server = ["/usr/libexec/sftp-server", "/usr/lib/openssh/sftp-server", "/usr/libexec/openssh/sftp-server"]
                .into_iter()
                .map(Path::new)
                .find(|p| p.exists())?;
            let dir = std::env::temp_dir().join(format!("xh-sftp-{}", std::process::id()));
            std::fs::remove_dir_all(&dir).ok();
            std::fs::create_dir_all(&dir).ok()?;
            for key in ["host_key", "client_key"] {
                let made = std::process::Command::new("ssh-keygen")
                    .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                    .arg(dir.join(key))
                    .status()
                    .ok()?;
                if !made.success() {
                    return None;
                }
            }
            std::fs::copy(dir.join("client_key.pub"), dir.join("authorized_keys")).ok()?;
            let port = std::net::TcpListener::bind("127.0.0.1:0").ok()?.local_addr().ok()?.port();
            let config = format!(
                "Port {port}\nListenAddress 127.0.0.1\nHostKey {dir}/host_key\nAuthorizedKeysFile {dir}/authorized_keys\n\
                 PasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nStrictModes no\n\
                 PidFile {dir}/sshd.pid\nSubsystem sftp {sftp}\nLogLevel ERROR\n",
                dir = dir.display(),
                sftp = sftp_server.display(),
            );
            std::fs::write(dir.join("sshd_config"), config).ok()?;
            let child = std::process::Command::new(sshd)
                .args(["-D", "-e", "-f"])
                .arg(dir.join("sshd_config"))
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .ok()?;
            let mut server = TestSshd { dir, port, child };
            // Wait for it to listen (or to give up).
            for _ in 0..100 {
                if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    return Some(server);
                }
                if server.child.try_wait().ok().flatten().is_some() {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            None
        }
    }

    impl Drop for TestSshd {
        fn drop(&mut self) {
            self.child.kill().ok();
            self.child.wait().ok();
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    #[test]
    fn sites_round_trip_as_json() {
        let site = Site {
            host: "h".into(),
            port: 22,
            user: "u".into(),
            auth: Auth::KeyFile { path: "~/.ssh/id_ed25519".into(), passphrase: String::new() },
        };
        let json = serde_json::to_string(&site).unwrap();
        assert!(json.contains(r#""kind":"key-file""#), "{json}");
        assert_eq!(serde_json::from_str::<Site>(&json).unwrap(), site);
        // Older or hand-written files can leave things out.
        assert_eq!(serde_json::from_str::<Site>(r#"{"host":"h"}"#).unwrap().auth, Auth::Agent);
    }
}

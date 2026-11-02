//! The server: a background process that does the app's work, so it keeps
//! going when the app quits. It owns every terminal's shell, and runs what
//! the app asks for (files, git, lino; see `rpc.rs`). The
//! app is a client: quit it, and shells, agents, and anything it started
//! (a push, a commit) keep running; open it again and it picks up where they
//! are. (Language servers run in the app.)
//!
//! The server is this same binary, started with `--server`. It runs on this
//! machine for now (and anyone who can reach its socket can use it: there's
//! no authentication yet). It owns a
//! PTY per session and keeps the last few MiB of each one's output. The app
//! attaches to a session over a Unix socket: the host replays the buffered
//! output (so the app rebuilds the screen and scrollback as it was), then
//! streams live output, and takes input and resizes back. (Windows 10 and
//! later have Unix sockets too; there the shells run on ConPTY, see
//! `pty.rs`.)
//!
//! - Quitting the app only detaches: sessions keep running.
//! - Closing a window kills its session (the app sends `KILL`).
//! - A shell that exits ends its session and tells the attached app.
//! - The host leaves when it has had no sessions and no app attached for a
//!   minute. It runs in its own process session (no controlling terminal),
//!   so closing the terminal that started the app doesn't end it.
//!
//! What can't survive: a reboot (or the host being killed). Then each window
//! starts a fresh shell in its last folder (see `ProjectItem::cwd`).
//!
//! Wire format: frames of `kind: u8`, `len: u32` (big-endian), payload.

use std::{
    collections::{HashMap, VecDeque},
    fs::File,
    io::{self, Read, Write},
    net::Shutdown,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use alacritty_terminal::event::WindowSize;
use serde::{Deserialize, Serialize};

#[cfg(unix)]
pub use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(windows)]
pub use uds_windows::{UnixListener, UnixStream};

use crate::services::pty::{Reader, ShellPty, Writer};
pub use crate::services::transport::{ServerKey, Stream};
use crate::services::transport;

/// Bump on any change to the frames or their payloads. It's part of the
/// socket's name, so an app never talks to an older host.
pub const PROTOCOL: u32 = 2;
const HOST_FLAG: &str = "--server";
/// What this process was started with before it was the server.
const OLD_HOST_FLAG: &str = "--session-host";
/// Output kept per session for replay: about 10k lines of colored text.
const REPLAY_LIMIT: usize = 4 << 20;
const IDLE_EXIT: Duration = Duration::from_secs(60);
const MAX_FRAME: usize = 64 << 20;

/// Frame kinds.
pub mod frame {
    // App → host.
    pub const HELLO: u8 = 1;
    pub const INPUT: u8 = 2;
    pub const RESIZE: u8 = 3;
    pub const KILL: u8 = 4;
    // Host → app.
    pub const ATTACHED: u8 = 10;
    pub const OUTPUT: u8 = 11;
    /// Everything before this was replayed history; after it, live output.
    pub const REPLAY_END: u8 = 12;
    pub const EXITED: u8 = 13;
    /// Another window attached to this session and took it over.
    pub const TAKEN_OVER: u8 = 14;
    // Calls (see rpc.rs), both ways on their own connection.
    /// App → server: `{id, method, params}`; `id` 0 wants no reply.
    pub const CALL: u8 = 20;
    /// Server → app: `{id, result}` or `{id, error}`.
    pub const REPLY: u8 = 21;
}

pub fn write_frame(writer: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    // One write per frame, so frames from different threads never interleave.
    let mut bytes = Vec::with_capacity(5 + payload.len());
    bytes.push(kind);
    bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    bytes.extend_from_slice(payload);
    writer.write_all(&bytes)
}

pub fn read_frame(reader: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    reader.read_exact(&mut header)?;
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut payload = vec![0; len];
    reader.read_exact(&mut payload)?;
    Ok((header[0], payload))
}

/// The first frame on every connection.
#[derive(Debug, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub session: String,
    /// Where to start the shell if the session doesn't exist yet.
    pub folder: Option<PathBuf>,
    pub columns: u16,
    pub lines: u16,
    /// `false`: just kill the session.
    pub attach: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Attached {
    /// The session was started for this attach (rather than resumed).
    pub created: bool,
}

pub fn encode_size(size: WindowSize) -> [u8; 8] {
    let mut bytes = [0u8; 8];
    bytes[0..2].copy_from_slice(&size.num_cols.to_be_bytes());
    bytes[2..4].copy_from_slice(&size.num_lines.to_be_bytes());
    bytes[4..6].copy_from_slice(&size.cell_width.to_be_bytes());
    bytes[6..8].copy_from_slice(&size.cell_height.to_be_bytes());
    bytes
}

fn decode_size(bytes: &[u8]) -> Option<WindowSize> {
    let field = |i: usize| Some(u16::from_be_bytes([*bytes.get(i)?, *bytes.get(i + 1)?]));
    Some(WindowSize {
        num_cols: field(0)?,
        num_lines: field(2)?,
        cell_width: field(4)?,
        cell_height: field(6)?,
    })
}

fn grid_size(columns: u16, lines: u16) -> WindowSize {
    WindowSize {
        num_lines: lines.max(1),
        num_cols: columns.max(2),
        cell_width: 8,
        cell_height: 17,
    }
}

// The app side ---------------------------------------------------------------

/// Where to reach a server.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum HostAddress {
    /// This machine's, on a Unix socket; started on demand if `can_start`.
    Local { socket: PathBuf, can_start: bool },
    /// Another machine's, over TCP with its key.
    Remote(ServerKey),
}

impl Default for HostAddress {
    /// This machine's server, started on demand.
    fn default() -> Self {
        HostAddress::Local {
            socket: local_socket(),
            can_start: true,
        }
    }
}

impl HostAddress {
    /// This machine's server, only if it's running.
    pub fn local_if_running() -> Self {
        HostAddress::Local {
            socket: local_socket(),
            can_start: false,
        }
    }

    fn without_starting(&self) -> Self {
        match self {
            HostAddress::Local { socket, .. } => HostAddress::Local { socket: socket.clone(), can_start: false },
            remote => remote.clone(),
        }
    }
}

fn local_socket() -> PathBuf {
    host_dir().join(format!("server-{PROTOCOL}.sock"))
}

fn host_dir() -> PathBuf {
    crate::services::paths::data_dir().unwrap_or_else(std::env::temp_dir)
}

/// Whether this process was started to be the host (see `main`).
pub fn is_host_invocation() -> bool {
    matches!(std::env::args().nth(1).as_deref(), Some(HOST_FLAG | OLD_HOST_FLAG))
}

pub(crate) fn connect(address: &HostAddress) -> io::Result<Stream> {
    let (socket, can_start) = match address {
        HostAddress::Remote(key) => return transport::connect(key).map(Stream::Remote),
        HostAddress::Local { socket, can_start } => (socket, *can_start),
    };
    match UnixStream::connect(socket) {
        Ok(stream) => return Ok(Stream::Local(stream)),
        Err(error) if !can_start => return Err(error),
        Err(_) => start_host()?,
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match UnixStream::connect(socket) {
            Ok(stream) => return Ok(Stream::Local(stream)),
            Err(error) if Instant::now() > deadline => return Err(error),
            Err(_) => thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// Starts this machine's server in the background (it outlives the app).
pub fn start_host() -> io::Result<()> {
    let dir = host_dir();
    std::fs::create_dir_all(&dir)?;
    let log = File::create(dir.join("server.log"))?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg(HOST_FLAG)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);
    // SAFETY: setsid is async-signal-safe, as pre_exec requires. A process
    // session of its own means no controlling terminal: closing the
    // terminal the app was started from doesn't hang up the shells.
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    // On Windows: no console, and outside the app's process group, so it
    // outlives the app.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    let mut host = command.spawn()?;
    // Reap it if it ever exits while the app runs.
    thread::spawn(move || host.wait().ok());
    Ok(())
}

/// Attaches to `hello.session`, creating it if needed. The stream then
/// carries frames: `OUTPUT` (replay first), `REPLAY_END`, then live output.
pub fn attach(address: &HostAddress, hello: &Hello) -> io::Result<(Stream, Attached)> {
    let mut stream = connect(address)?;
    write_frame(&mut stream, frame::HELLO, &serde_json::to_vec(hello)?)?;
    let (kind, payload) = read_frame(&mut stream)?;
    if kind != frame::ATTACHED {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected reply"));
    }
    Ok((stream, serde_json::from_slice(&payload)?))
}

/// Ends a session, if the host is running and has it.
pub fn kill(address: &HostAddress, session: &str) {
    let Ok(mut stream) = connect(&address.without_starting()) else {
        return;
    };
    let hello = Hello {
        protocol: PROTOCOL,
        session: session.to_string(),
        folder: None,
        columns: 0,
        lines: 0,
        attach: false,
    };
    if let Ok(payload) = serde_json::to_vec(&hello) {
        write_frame(&mut stream, frame::HELLO, &payload).ok();
        // Wait for the host to hang up, so the kill is done when we return.
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest).ok();
    }
}

// The host side --------------------------------------------------------------

/// The server's options (`xharness --server …`).
#[derive(Default)]
struct Options {
    /// Also take remote connections here (`--listen 0.0.0.0:38888`).
    listen: Option<String>,
    /// The host name the printed key gives (`--host`): how the app reaches
    /// this machine.
    host: Option<String>,
    name: Option<String>,
    new_key: bool,
    stop: bool,
}

fn options() -> Result<Options, String> {
    let mut options = Options::default();
    // After the program (and `--server`, which the app's binary needs).
    let mut args = std::env::args().skip(1).filter(|arg| arg != HOST_FLAG && arg != OLD_HOST_FLAG);
    while let Some(arg) = args.next() {
        let mut value = |flag: &str| args.next().ok_or(format!("{flag} needs a value"));
        match arg.as_str() {
            "--listen" => options.listen = Some(value("--listen")?),
            "--host" => options.host = Some(value("--host")?),
            "--name" => options.name = Some(value("--name")?),
            "--new-key" => options.new_key = true,
            "--stop" => options.stop = true,
            "--help" | "-h" => {
                return Err("usage: xharness --server [--listen ADDRESS:PORT] [--host HOST] [--name NAME] [--new-key]\n       xharness --server --stop\n(the server-only build takes the same options, without --server)".to_string());
            }
            other => return Err(format!("unknown option {other} (see --help)")),
        }
    }
    Ok(options)
}

/// Runs the server (the `--server` process). Started by the app it serves
/// only this machine and leaves when idle; with `--listen` it takes
/// remote connections (with its key) and keeps running.
pub fn run_host() -> io::Result<()> {
    let options = options().map_err(io::Error::other)?;
    let dir = host_dir();
    std::fs::create_dir_all(&dir)?;
    if options.stop {
        return stop_running_server();
    }
    crate::services::rpc::enter_server_mode();
    let socket = local_socket();
    let lock = dir.join(format!("server-{PROTOCOL}.lock"));
    let host = Arc::new(Host::default());
    let Some(listen) = options.listen.clone() else {
        return serve_with(host, &socket, &lock, Some(IDLE_EXIT));
    };

    let address: std::net::SocketAddr = listen
        .parse()
        .map_err(|_| io::Error::other(format!("--listen wants ADDRESS:PORT, like 0.0.0.0:38888 (got {listen})")))?;
    let lock_file = File::create(&lock)?;
    if lock_file.try_lock().is_err() {
        return Err(io::Error::other(
            "a server is already running on this machine (the app starts one); stop it with `xharness --server --stop` and try again",
        ));
    }
    drop(lock_file);
    let key = server_key(&dir, &options, address)?;
    let listener = std::net::TcpListener::bind(address)?;
    println!("XHarness server, protocol {PROTOCOL}, on {address} (and this machine's socket).");
    println!();
    println!("Connection key: in XHarness, choose New project → Remote and paste it:");
    println!();
    println!("  {}", key.encode());
    println!();
    println!("It reaches {} as \"{}\" (change it with --host and --name).", key.address(), key.name);
    println!("Anyone with this key can run anything on this machine as you. Keep it");
    println!("secret; --new-key makes a new one (and the old one stops working).");
    let _ = LISTENING.set(address.to_string());
    {
        let host = host.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else {
                    continue;
                };
                let (host, key) = (host.clone(), key.clone());
                thread::spawn(move || {
                    let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
                    match transport::accept(stream, &key) {
                        Ok(secure) => {
                            if let Err(error) = host.serve_client(Stream::Remote(secure)) {
                                eprintln!("xharness server: {peer}: {error}");
                            }
                        }
                        Err(error) => eprintln!("xharness server: turned away {peer}: {error}"),
                    }
                });
            }
        });
    }
    // Remote connections are served even if this machine's socket can't be
    // (a data folder path too long for one, say).
    if let Err(error) = serve_with(host, &socket, &lock, None) {
        eprintln!("xharness server: this machine's socket: {error}; serving remote connections only");
        loop {
            thread::park();
        }
    }
    Ok(())
}

/// The key the server prints: saved in the data folder (only its secret),
/// so it stays the same across restarts until `--new-key`.
fn server_key(dir: &Path, options: &Options, address: std::net::SocketAddr) -> io::Result<ServerKey> {
    let path = dir.join("server-key.json");
    let name = options.name.clone().unwrap_or_else(hostname);
    let host = options.host.clone().unwrap_or_else(|| {
        if address.ip().is_unspecified() { hostname() } else { address.ip().to_string() }
    });
    let saved: Option<String> = (!options.new_key)
        .then(|| std::fs::read_to_string(&path).ok())
        .flatten()
        .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
        .and_then(|value| value["secret"].as_str().map(String::from));
    let secret = match saved {
        Some(secret) => secret,
        None => {
            let secret = ServerKey::generate(&name, &host, address.port()).map_err(io::Error::other)?.secret;
            std::fs::write(&path, serde_json::json!({ "secret": secret }).to_string())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            }
            secret
        }
    };
    Ok(ServerKey {
        name,
        host,
        port: address.port(),
        secret,
    })
}

fn stop_running_server() -> io::Result<()> {
    let mut stream = connect(&HostAddress::local_if_running())
        .map_err(|_| io::Error::other("no server is running on this machine"))?;
    let call = serde_json::json!({ "id": 1, "method": "server.shutdown", "params": null });
    write_frame(&mut stream, frame::CALL, &serde_json::to_vec(&call)?)?;
    read_frame(&mut stream).ok();
    println!("Stopped the server.");
    Ok(())
}

/// Serves sessions on `socket` until idle for `idle_exit` (forever if
/// `None`). Returns at once if another host holds `lock`.
#[cfg(test)]
pub fn serve(socket: &Path, lock: &Path, idle_exit: Option<Duration>) -> io::Result<()> {
    serve_with(Arc::new(Host::default()), socket, lock, idle_exit)
}

fn serve_with(host: Arc<Host>, socket: &Path, lock: &Path, idle_exit: Option<Duration>) -> io::Result<()> {
    let lock_file = File::create(lock)?;
    // Held for as long as this host runs; another host already has it if
    // this fails.
    if lock_file.try_lock().is_err() {
        return Ok(());
    }
    // We hold the lock, so any socket file is a dead host's.
    std::fs::remove_file(socket).ok();
    let _ = HOST.set(host.clone());
    let listener = UnixListener::bind(socket)?;
    if let Some(idle) = idle_exit {
        let host = host.clone();
        let socket = socket.to_path_buf();
        thread::spawn(move || host.exit_when_idle(idle, &socket));
    }
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        let host = host.clone();
        thread::spawn(move || {
            if let Err(error) = host.serve_client(Stream::Local(stream)) {
                eprintln!("xharness server: {error}");
            }
        });
    }
    drop(lock_file);
    Ok(())
}

/// The running server, for `server.info`.
static HOST: std::sync::OnceLock<Arc<Host>> = std::sync::OnceLock::new();
static STARTED: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
/// Where it takes remote connections, if it does.
static LISTENING: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// What a server says about itself (the app shows it in the footer).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerInfo {
    pub hostname: String,
    pub os: String,
    pub version: String,
    pub protocol: u32,
    /// Terminals running.
    pub sessions: usize,
    /// Apps (and terminal windows) connected.
    pub clients: usize,
    pub uptime_s: u64,
    pub listening: Option<String>,
}

pub fn info() -> ServerInfo {
    let host = HOST.get();
    ServerInfo {
        hostname: hostname(),
        os: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        version: env!("CARGO_PKG_VERSION").to_string(),
        protocol: PROTOCOL,
        sessions: host.map_or(0, |h| h.sessions.lock().unwrap().len()),
        clients: host.map_or(0, |h| h.clients.load(Ordering::SeqCst)),
        uptime_s: STARTED.get_or_init(Instant::now).elapsed().as_secs(),
        listening: LISTENING.get().cloned(),
    }
}

/// The server's own calls (see `rpc.rs`).
pub(crate) fn dispatch(method: &str, params: serde_json::Value) -> Option<Result<serde_json::Value, String>> {
    use crate::services::rpc::handle_call;
    Some(match method {
        "server.info" => handle_call(params, |(): ()| info()),
        // Ends the server, and every shell in it, once the reply is out.
        "server.shutdown" => handle_call(params, |(): ()| {
            thread::spawn(|| {
                thread::sleep(Duration::from_millis(150));
                std::process::exit(0);
            });
            true
        }),
        _ => return None,
    })
}

/// This machine's name.
pub fn hostname() -> String {
    #[cfg(unix)]
    {
        let mut buffer = [0u8; 256];
        // SAFETY: gethostname writes at most `len` bytes into the buffer.
        if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } == 0 {
            let len = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
            let name = String::from_utf8_lossy(&buffer[..len]).into_owned();
            return name.trim_end_matches(".local").to_string();
        }
    }
    std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "server".to_string())
}

#[derive(Default)]
struct Host {
    sessions: Mutex<HashMap<String, Arc<HostSession>>>,
    clients: AtomicUsize,
    next_client: AtomicU64,
}

/// Counts a connected app for as long as it lives.
struct ClientGuard<'a>(&'a AtomicUsize);

impl Drop for ClientGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Host {
    fn serve_client(self: &Arc<Self>, stream: Stream) -> io::Result<()> {
        self.clients.fetch_add(1, Ordering::SeqCst);
        let _guard = ClientGuard(&self.clients);

        let mut reader = stream.try_clone()?;
        let (kind, payload) = read_frame(&mut reader)?;
        // A connection that starts with a call is the app's call channel.
        if kind == frame::CALL {
            return crate::services::rpc::serve(stream, payload);
        }
        if kind != frame::HELLO {
            return Ok(());
        }
        let hello: Hello = serde_json::from_slice(&payload)?;
        if hello.protocol != PROTOCOL {
            return Ok(());
        }
        if !hello.attach {
            self.kill(&hello.session);
            return Ok(());
        }

        let (session, created) = self.open(&hello)?;
        let mut writer = stream.try_clone()?;
        write_frame(&mut writer, frame::ATTACHED, &serde_json::to_vec(&Attached { created })?)?;
        let client = self.next_client.fetch_add(1, Ordering::SeqCst);
        session.attach(client, writer)?;

        loop {
            let Ok((kind, payload)) = read_frame(&mut reader) else {
                break;
            };
            match kind {
                frame::INPUT => session.write(&payload),
                frame::RESIZE => {
                    if let Some(size) = decode_size(&payload) {
                        session.resize(size);
                    }
                }
                frame::KILL => {
                    self.kill(&session.id);
                    break;
                }
                _ => {}
            }
        }
        session.detach(client);
        Ok(())
    }

    /// The session, started now if it doesn't exist.
    fn open(self: &Arc<Self>, hello: &Hello) -> io::Result<(Arc<HostSession>, bool)> {
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(session) = sessions.get(&hello.session) {
            return Ok((session.clone(), false));
        }
        let (pty, output, input) = ShellPty::spawn(hello.folder.clone(), grid_size(hello.columns, hello.lines))?;
        let session = Arc::new(HostSession {
            id: hello.session.clone(),
            pty: Mutex::new(Some(pty)),
            input: Mutex::new(input),
            state: Mutex::new(SessionState::default()),
        });
        sessions.insert(hello.session.clone(), session.clone());

        let host = self.clone();
        let pump = session.clone();
        thread::spawn(move || {
            pump.pump(output);
            host.remove_ended(&pump);
        });
        Ok((session, true))
    }

    /// Forgets a session whose shell ended, unless it's gone already. By
    /// then a new session may have the same name (killed, then opened
    /// again: on Windows the old output only ends once its console has
    /// closed), and that one stays.
    fn remove_ended(&self, ended: &Arc<HostSession>) {
        let mut sessions = self.sessions.lock().unwrap();
        let session = match sessions.get(&ended.id) {
            Some(current) if Arc::ptr_eq(current, ended) => sessions.remove(&ended.id),
            _ => None,
        };
        drop(sessions);
        Self::hang_up(session);
    }

    fn kill(&self, id: &str) {
        let session = self.sessions.lock().unwrap().remove(id);
        Self::hang_up(session);
    }

    /// Dropping the PTY hangs up on the shell and reaps it. Outside the
    /// sessions lock: that can take a moment.
    fn hang_up(session: Option<Arc<HostSession>>) {
        if let Some(session) = session {
            session.pty.lock().unwrap().take();
        }
    }

    fn exit_when_idle(&self, idle: Duration, socket: &Path) {
        let mut idle_since = Instant::now();
        loop {
            thread::sleep(Duration::from_secs(2));
            let busy = !self.sessions.lock().unwrap().is_empty()
                || self.clients.load(Ordering::SeqCst) > 0;
            if busy {
                idle_since = Instant::now();
            } else if idle_since.elapsed() >= idle {
                std::fs::remove_file(socket).ok();
                std::process::exit(0);
            }
        }
    }
}

struct HostSession {
    id: String,
    /// `None` once killed.
    pty: Mutex<Option<ShellPty>>,
    input: Mutex<Writer>,
    state: Mutex<SessionState>,
}

#[derive(Default)]
struct SessionState {
    /// The most recent output, for replay.
    replay: VecDeque<u8>,
    /// The attached app connection, if any.
    client: Option<(u64, Stream)>,
    exited: bool,
}

impl HostSession {
    /// Reads the shell's output until it exits.
    fn pump(&self, mut output: Reader) {
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            match output.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => self.output(&buffer[..n]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                // EIO once the shell is gone.
                Err(_) => break,
            }
        }
        let mut state = self.state.lock().unwrap();
        state.exited = true;
        if let Some((_, client)) = state.client.as_mut() {
            write_frame(client, frame::EXITED, &[]).ok();
        }
    }

    fn output(&self, bytes: &[u8]) {
        let mut state = self.state.lock().unwrap();
        push_replay(&mut state.replay, bytes, REPLAY_LIMIT);
        if let Some((_, client)) = state.client.as_mut()
            && write_frame(client, frame::OUTPUT, bytes).is_err()
        {
            state.client = None;
        }
    }

    /// Replays what the session printed so far, then streams to `stream`.
    /// One app window per session: attaching takes it over from any other.
    fn attach(&self, client: u64, mut stream: Stream) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if let Some((_, mut previous)) = state.client.take() {
            write_frame(&mut previous, frame::TAKEN_OVER, &[]).ok();
            previous.shutdown(Shutdown::Both).ok();
        }
        let (front, back) = state.replay.as_slices();
        let mut replay = Vec::with_capacity(front.len() + back.len());
        replay.extend_from_slice(front);
        replay.extend_from_slice(back);
        write_frame(&mut stream, frame::OUTPUT, &replay)?;
        write_frame(&mut stream, frame::REPLAY_END, &[])?;
        if state.exited {
            write_frame(&mut stream, frame::EXITED, &[])?;
        }
        state.client = Some((client, stream));
        Ok(())
    }

    fn detach(&self, client: u64) {
        let mut state = self.state.lock().unwrap();
        if state.client.as_ref().is_some_and(|(id, _)| *id == client) {
            state.client = None;
        }
    }

    fn write(&self, bytes: &[u8]) {
        self.input.lock().unwrap().write_all(bytes).ok();
    }

    fn resize(&self, size: WindowSize) {
        if let Some(pty) = self.pty.lock().unwrap().as_mut() {
            pty.resize(size);
        }
    }
}

/// Appends to the replay buffer, dropping the oldest output past `limit`.
/// The cut moves on to the next line break, so replay doesn't begin in the
/// middle of an escape sequence.
fn push_replay(replay: &mut VecDeque<u8>, bytes: &[u8], limit: usize) {
    replay.extend(bytes);
    if replay.len() <= limit {
        return;
    }
    replay.drain(..replay.len() - limit);
    if let Some(newline) = replay.iter().position(|&b| b == b'\n') {
        replay.drain(..=newline);
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A host serving in this process on a fresh socket.
    pub fn start_test_host(name: &str) -> HostAddress {
        let dir = std::env::temp_dir().join(format!("xh-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("s.sock");
        let lock = dir.join("s.lock");
        {
            let socket = socket.clone();
            thread::spawn(move || serve(&socket, &lock, None).unwrap());
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while UnixStream::connect(&socket).is_err() {
            assert!(Instant::now() < deadline, "test host didn't start");
            thread::sleep(Duration::from_millis(10));
        }
        HostAddress::Local {
            socket,
            can_start: false,
        }
    }

    /// A server in this process taking encrypted TCP connections on a free
    /// port, with a key of its own.
    pub fn start_test_remote(name: &str) -> HostAddress {
        let host = Arc::new(Host::default());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let key = ServerKey::generate(name, "127.0.0.1", listener.local_addr().unwrap().port()).unwrap();
        let server_key = key.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (host, key) = (host.clone(), server_key.clone());
                thread::spawn(move || {
                    if let Ok(secure) = transport::accept(stream, &key) {
                        host.serve_client(Stream::Remote(secure)).ok();
                    }
                });
            }
        });
        HostAddress::Remote(key)
    }

    /// A command for the shell sessions run: zsh (bash where there's no
    /// zsh; these work in both), or PowerShell on Windows.
    /// Tests wait for output only the command's result contains (`$((1+1))`
    /// prints `2`), not the typed line the shell echoes back.
    pub fn shell_command(zsh: &'static str, powershell: &'static str) -> &'static [u8] {
        if cfg!(windows) { powershell.as_bytes() } else { zsh.as_bytes() }
    }

    fn hello(session: &str, folder: Option<PathBuf>) -> Hello {
        Hello {
            protocol: PROTOCOL,
            session: session.into(),
            folder,
            columns: 80,
            lines: 24,
            attach: true,
        }
    }

    /// Reads frames until the output so far contains `needle`. Returns all
    /// output read (replay included).
    fn read_until(stream: &mut Stream, needle: &str) -> String {
        if let Stream::Local(local) = stream {
            local.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
        }
        let mut output = Vec::new();
        while !String::from_utf8_lossy(&output).contains(needle) {
            match read_frame(stream) {
                Ok((frame::OUTPUT, bytes)) => output.extend(bytes),
                Ok(_) => {}
                Err(error) => panic!(
                    "{error} before {needle:?}; output was:\n{}",
                    String::from_utf8_lossy(&output)
                ),
            }
        }
        String::from_utf8_lossy(&output).into_owned()
    }

    #[test]
    fn frames_round_trip() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, frame::INPUT, b"ls\r").unwrap();
        write_frame(&mut bytes, frame::REPLAY_END, &[]).unwrap();
        let mut reader = bytes.as_slice();
        assert_eq!(read_frame(&mut reader).unwrap(), (frame::INPUT, b"ls\r".to_vec()));
        assert_eq!(read_frame(&mut reader).unwrap(), (frame::REPLAY_END, vec![]));
        let size = WindowSize { num_cols: 120, num_lines: 40, cell_width: 8, cell_height: 17 };
        let decoded = decode_size(&encode_size(size)).unwrap();
        assert_eq!((decoded.num_cols, decoded.num_lines), (120, 40));
    }

    #[test]
    fn replay_keeps_the_newest_output_from_a_line_start() {
        let mut replay = VecDeque::new();
        push_replay(&mut replay, b"old line\n\x1b[31mnew", 12);
        // Over the limit: cut to the last 12 bytes, then to after the
        // newline, so the escape sequence is whole.
        assert_eq!(replay.iter().copied().collect::<Vec<_>>(), b"\x1b[31mnew");
    }

    /// The point of the host: detaching leaves the shell running, with its
    /// state, and reattaching shows what it printed.
    #[test]
    fn sessions_survive_detaching() {
        let address = start_test_host("survive");

        let (mut first, attached) = attach(&address, &hello("keep", None)).unwrap();
        assert!(attached.created);
        let set = shell_command("export XH_STATE=kept-$((6*7))\r", "$env:XH_STATE = \"kept-$(6*7)\"\r");
        write_frame(&mut first, frame::INPUT, set).unwrap();
        write_frame(&mut first, frame::INPUT, shell_command("echo marker-$((1+1))\r", "echo \"marker-$(1+1)\"\r")).unwrap();
        read_until(&mut first, "marker-2");
        drop(first); // The app quits.

        let (mut second, attached) = attach(&address, &hello("keep", None)).unwrap();
        assert!(!attached.created, "the same session comes back");
        // The replay has the earlier output...
        let replay = read_until(&mut second, "marker-2");
        assert!(replay.contains("XH_STATE"));
        // ...and it's the same shell: the variable is still set.
        let show = shell_command("echo state-$XH_STATE\r", "echo \"state-$env:XH_STATE\"\r");
        write_frame(&mut second, frame::INPUT, show).unwrap();
        read_until(&mut second, "state-kept-42");
    }

    /// A killed session's output can end after a new one took its name;
    /// the new one stays.
    #[test]
    fn an_old_session_ending_leaves_its_successor() {
        let host = Host::default();
        let session = || {
            Arc::new(HostSession {
                id: "again".into(),
                pty: Mutex::new(None),
                input: Mutex::new(Box::new(io::sink())),
                state: Mutex::new(SessionState::default()),
            })
        };
        let (old, new) = (session(), session());
        host.sessions.lock().unwrap().insert("again".into(), old.clone());
        host.kill("again");
        host.sessions.lock().unwrap().insert("again".into(), new.clone());
        host.remove_ended(&old);
        assert!(Arc::ptr_eq(&host.sessions.lock().unwrap()["again"], &new));
        host.remove_ended(&new);
        assert!(host.sessions.lock().unwrap().is_empty());
    }

    #[test]
    fn killing_ends_the_session() {
        let address = start_test_host("kill");
        let (mut stream, _) = attach(&address, &hello("doomed", None)).unwrap();
        write_frame(&mut stream, frame::INPUT, shell_command("export XH_STATE=old\r", "$env:XH_STATE = \"old\"\r")).unwrap();
        write_frame(&mut stream, frame::INPUT, shell_command("echo ready-$((1+1))\r", "echo \"ready-$(1+1)\"\r")).unwrap();
        read_until(&mut stream, "ready-2");
        drop(stream);

        kill(&address, "doomed");
        let (mut stream, attached) = attach(&address, &hello("doomed", None)).unwrap();
        assert!(attached.created, "a new shell, not the killed one");
        let show = shell_command("echo state-[$XH_STATE]\r", "echo \"state-[$env:XH_STATE]\"\r");
        write_frame(&mut stream, frame::INPUT, show).unwrap();
        read_until(&mut stream, "state-[]");
    }

    #[test]
    fn new_sessions_start_in_their_folder() {
        let address = start_test_host("folder");
        let folder = std::env::temp_dir().join(format!("xh-folder-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let (mut stream, _) = attach(&address, &hello("here", Some(folder.clone()))).unwrap();
        let show = shell_command("echo in-$(basename \"$PWD\")\r", "echo \"in-$(Split-Path -Leaf $PWD)\"\r");
        write_frame(&mut stream, frame::INPUT, show).unwrap();
        read_until(&mut stream, &format!("in-{}", folder.file_name().unwrap().to_string_lossy()));
    }

    #[test]
    fn an_exiting_shell_reports_it() {
        let address = start_test_host("exit");
        let (mut stream, _) = attach(&address, &hello("brief", None)).unwrap();
        // Once it's reading input, so the exit isn't typed ahead of startup.
        write_frame(&mut stream, frame::INPUT, shell_command("echo ready-$((1+1))\r", "echo \"ready-$(1+1)\"\r")).unwrap();
        let mut output = read_until(&mut stream, "ready-2");
        write_frame(&mut stream, frame::INPUT, b"exit\r").unwrap();
        if let Stream::Local(local) = &stream {
            local.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
        }
        loop {
            match read_frame(&mut stream) {
                Ok((frame::EXITED, _)) => break,
                Ok((frame::OUTPUT, bytes)) => output.push_str(&String::from_utf8_lossy(&bytes)),
                Ok(_) => {}
                Err(error) => panic!("no EXITED frame: {error}; output was:\n{output}"),
            }
        }
    }
}

//! Calls from the app to the server (see `server.rs`), so the work happens
//! in the server: it keeps going when the app quits, and later it can run
//! on another machine.
//!
//! A service function names itself and its arguments and says what it does
//! (`route`). In the server that runs; in the app it becomes a call: the
//! arguments go as JSON, the server runs the same function, and the result
//! comes back. One connection carries every call, each with an id, so calls
//! from different threads run at the same time. A call with id 0 gets no
//! reply (the first frame, `rpc.hello`, which makes a connection the call
//! channel).
//!
//! Which server: this machine's, unless the caller says otherwise with
//! [`on`] (a remote project's windows wrap their work in it). Each server
//! has its own connection: a Unix socket here, an encrypted TCP connection
//! (see `transport.rs`) to another machine.
//!
//! Work never runs in the app: if its server can't be reached, the call
//! returns what "unreachable" means for it (an error, nothing), and the
//! app shows the project as not connected. This machine's server is
//! started when it's needed, unless you stopped it (see
//! [`set_local_autostart`]).

use std::{
    cell::RefCell,
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
};

use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::services::server::{self, HostAddress, ServerInfo, Stream, frame, read_frame, write_frame};

thread_local! {
    /// Where routed work goes on this thread (`None`: this machine).
    static TARGET: RefCell<Option<HostAddress>> = const { RefCell::new(None) };
}

/// Runs `f` with routed work going to `target` (this machine's server if
/// `None`).
pub fn on<R>(target: Option<&HostAddress>, f: impl FnOnce() -> R) -> R {
    let previous = TARGET.with(|t| t.replace(target.cloned()));
    let result = f();
    TARGET.with(|t| t.replace(previous));
    result
}

fn current() -> HostAddress {
    TARGET.with(|t| t.borrow().clone()).unwrap_or_else(local)
}

/// Whether calls to this machine's server may start it. Off until you
/// start it (the footer, or a project's "Start server"): the app doesn't
/// start a server behind your back; stopping it turns this off again.
static LOCAL_AUTOSTART: AtomicBool = AtomicBool::new(false);

pub fn set_local_autostart(on: bool) {
    LOCAL_AUTOSTART.store(on, Ordering::SeqCst);
}

/// This machine's server, started on demand unless you stopped it.
pub fn local_address() -> HostAddress {
    local()
}

fn local() -> HostAddress {
    if LOCAL_AUTOSTART.load(Ordering::SeqCst) {
        HostAddress::default()
    } else {
        HostAddress::local_if_running()
    }
}

/// What a routed call returns when its (remote) server can't be reached.
pub trait Unreachable {
    fn unreachable(error: String) -> Self;
}

impl<T> Unreachable for Result<T, String> {
    fn unreachable(error: String) -> Self {
        Err(error)
    }
}

impl<T> Unreachable for Option<T> {
    fn unreachable(_: String) -> Self {
        None
    }
}

impl<T> Unreachable for Vec<T> {
    fn unreachable(_: String) -> Self {
        Vec::new()
    }
}

impl<T> Unreachable for (Vec<T>, bool) {
    fn unreachable(_: String) -> Self {
        (Vec::new(), false)
    }
}

static SERVER_MODE: AtomicBool = AtomicBool::new(false);

/// Called once in the server process: from now on, routed work runs here.
pub fn enter_server_mode() {
    SERVER_MODE.store(true, Ordering::SeqCst);
}

pub fn in_server() -> bool {
    SERVER_MODE.load(Ordering::SeqCst)
}

/// Runs `here` in the server (and in tests); in the app, asks the server to
/// run `method` with `args` (which must be what the server's `dispatch`
/// expects for it) and returns its result.
pub fn route<A: Serialize, R: Serialize + DeserializeOwned + Unreachable>(
    method: &str,
    args: A,
    here: impl FnOnce() -> R,
) -> R {
    if in_server() || cfg!(test) {
        return here();
    }
    let target = current();
    let result = serde_json::to_value(&args)
        .map_err(|error| error.to_string())
        .and_then(|params| call_on(&target, method, params))
        .and_then(|value| serde_json::from_value(value).map_err(|error| error.to_string()));
    match (result, &target) {
        (Ok(result), _) => result,
        // It answered, but it's from before this call existed.
        (Err(error), HostAddress::Remote(key)) if error.contains("the server has no ") => R::unreachable(format!(
            "{} runs an older XHarness server without this: update it and start it again.",
            key.name
        )),
        (Err(error), HostAddress::Local { .. }) if error.contains("the server has no ") => R::unreachable(
            "This Mac's server is running an older XHarness: stop it and start it again (in the footer) to use this. Stopping ends its terminals."
                .to_string(),
        ),
        (Err(error), HostAddress::Remote(key)) => R::unreachable(format!("{} didn't answer: {error}", key.name)),
        (Err(error), HostAddress::Local { .. }) => R::unreachable(format!("the server isn't running: {error}")),
    }
}

/// How long `target` takes to answer, and what it says about itself.
/// Doesn't start this machine's server: `Err` if it isn't running.
pub fn ping(target: Option<&HostAddress>) -> Result<(std::time::Duration, ServerInfo), String> {
    let target = match target {
        Some(target) => target.clone(),
        None => HostAddress::local_if_running(),
    };
    let started = std::time::Instant::now();
    let info = call_on(&target, "server.info", Value::Null)?;
    let took = started.elapsed();
    Ok((took, serde_json::from_value(info).map_err(|error| error.to_string())?))
}

/// Stops this machine's server, and every shell in it.
pub fn stop_local_server() -> Result<(), String> {
    call_on(&HostAddress::local_if_running(), "server.shutdown", Value::Null).map(|_| ())
}

// The app's side -------------------------------------------------------------------

/// The app's connection to a server.
struct Connection {
    writer: Mutex<Stream>,
    pending: Mutex<HashMap<u64, mpsc::Sender<Result<Value, String>>>>,
    next_id: AtomicU64,
    alive: AtomicBool,
}

static CONNECTIONS: Mutex<Option<HashMap<HostAddress, Arc<Connection>>>> = Mutex::new(None);

/// The connection to `target`, opened (and this machine's server started)
/// if need be.
fn connection(target: &HostAddress) -> Result<Arc<Connection>, String> {
    let mut connections = CONNECTIONS.lock().unwrap();
    let connections = connections.get_or_insert_with(HashMap::new);
    if let Some(connection) = connections.get(target).filter(|c| c.alive.load(Ordering::SeqCst)) {
        return Ok(connection.clone());
    }
    let stream = server::connect(target).map_err(|error| error.to_string())?;
    let connection = Connection::open(stream)?;
    connections.insert(target.clone(), connection.clone());
    Ok(connection)
}

impl Connection {
    /// Makes `stream` (to a server) a call channel, reading replies on a
    /// thread of its own.
    fn open(stream: Stream) -> Result<Arc<Self>, String> {
        let reader = stream.try_clone().map_err(|error| error.to_string())?;
        let connection = Arc::new(Connection {
            writer: Mutex::new(stream),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            alive: AtomicBool::new(true),
        });
        // The first frame makes this the call channel.
        connection.send(0, "rpc.hello", &Value::Null).map_err(|error| error.to_string())?;
        let reading = connection.clone();
        thread::Builder::new()
            .name("xharness-rpc".into())
            .spawn(move || reading.read_loop(reader))
            .map_err(|error| error.to_string())?;
        Ok(connection)
    }

    /// Sends a call and waits for its reply. `Err(None)`: it never reached
    /// the server (so it can be sent again).
    fn call(&self, method: &str, params: &Value) -> Result<Value, Option<String>> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, sender);
        if self.send(id, method, params).is_err() {
            self.pending.lock().unwrap().remove(&id);
            return Err(None);
        }
        match receiver.recv() {
            Ok(reply) => reply.map_err(Some),
            Err(_) => Err(Some("lost the connection to the server".to_string())),
        }
    }

    fn send(&self, id: u64, method: &str, params: &Value) -> std::io::Result<()> {
        let payload = serde_json::to_vec(&json!({ "id": id, "method": method, "params": params }))?;
        let result = write_frame(&mut *self.writer.lock().unwrap(), frame::CALL, &payload);
        if result.is_err() {
            self.alive.store(false, Ordering::SeqCst);
        }
        result
    }

    fn read_loop(&self, mut reader: Stream) {
        while let Ok((kind, payload)) = read_frame(&mut reader) {
            let Ok(message) = serde_json::from_slice::<Value>(&payload) else {
                continue;
            };
            match kind {
                frame::REPLY => {
                    let id = message["id"].as_u64().unwrap_or(0);
                    if let Some(waiting) = self.pending.lock().unwrap().remove(&id) {
                        let reply = match message.get("error").and_then(Value::as_str) {
                            Some(error) => Err(error.to_string()),
                            None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        waiting.send(reply).ok();
                    }
                }
                _ => {}
            }
        }
        // The server went away: callers waiting hear so, and the next call
        // connects (and starts it) again.
        self.alive.store(false, Ordering::SeqCst);
        self.pending.lock().unwrap().clear();
    }
}

/// Calls `method` on `target` and waits for its reply.
fn call_on(target: &HostAddress, method: &str, params: Value) -> Result<Value, String> {
    // A call that never reached the server can be sent again on a new
    // connection; one that did isn't (it may have done something).
    for _ in 0..2 {
        match connection(target)?.call(method, &params) {
            Ok(result) => return Ok(result),
            Err(Some(error)) => return Err(error),
            Err(None) => continue,
        }
    }
    Err("can't reach the server".to_string())
}

// The server's side ------------------------------------------------------------------

type Writer = Arc<Mutex<Stream>>;

/// Serves one app's calls; `first` is the connection's first frame.
pub fn serve(stream: Stream, first: Vec<u8>) -> std::io::Result<()> {
    let writer: Writer = Arc::new(Mutex::new(stream.try_clone()?));
    let mut reader = stream;
    handle(first, &writer);
    while let Ok((kind, payload)) = read_frame(&mut reader) {
        if kind == frame::CALL {
            handle(payload, &writer);
        }
    }
    Ok(())
}

fn handle(payload: Vec<u8>, writer: &Writer) {
    let Ok(call) = serde_json::from_slice::<Value>(&payload) else {
        return;
    };
    let id = call["id"].as_u64().unwrap_or(0);
    let method = call["method"].as_str().unwrap_or_default().to_string();
    let params = call.get("params").cloned().unwrap_or(Value::Null);
    if id == 0 {
        // No reply wanted: run it now, so these stay in order.
        if method != "rpc.hello" {
            crate::services::dispatch(&method, params).ok();
        }
        return;
    }
    // Calls run side by side (a push can take a while).
    let writer = writer.clone();
    thread::spawn(move || {
        let reply = match crate::services::dispatch(&method, params) {
            Ok(result) => json!({ "id": id, "result": result }),
            Err(error) => json!({ "id": id, "error": error }),
        };
        if let Ok(bytes) = serde_json::to_vec(&reply) {
            write_frame(&mut *writer.lock().unwrap(), frame::REPLY, &bytes).ok();
        }
    });
}

/// For `dispatch` functions: reads the arguments, runs `f`, returns its
/// result as JSON.
pub fn handle_call<A: DeserializeOwned, R: Serialize>(params: Value, f: impl FnOnce(A) -> R) -> Result<Value, String> {
    let args: A = serde_json::from_value(params).map_err(|error| format!("bad arguments: {error}"))?;
    serde_json::to_value(f(args)).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real server on a socket of its own: calls go over the wire, run
    /// there, and come back, several at once.
    #[test]
    fn calls_run_in_the_server() {
        let address = server::tests::start_test_host("rpc");
        let dir = std::env::temp_dir().join(format!("xh-rpc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("note.txt");
        std::fs::write(&file, "hello from the server's disk").unwrap();

        let connection = Connection::open(server::connect(&address).unwrap()).unwrap();
        let read = connection.call("files.read", &json!([file])).unwrap();
        assert_eq!(read, json!({ "Ok": "hello from the server's disk" }));

        // Several calls at once, each answered by its id.
        let threads: Vec<_> = (0..8)
            .map(|n| {
                let (connection, dir) = (connection.clone(), dir.clone());
                thread::spawn(move || {
                    let path = dir.join(format!("{n}.txt"));
                    connection.call("files.write", &json!([path, format!("file {n}")])).unwrap();
                    connection.call("files.read", &json!([path])).unwrap()
                })
            })
            .collect();
        for (n, thread) in threads.into_iter().enumerate() {
            assert_eq!(thread.join().unwrap(), json!({ "Ok": format!("file {n}") }));
        }

        let unknown = connection.call("nothing.here", &Value::Null);
        assert!(matches!(unknown, Err(Some(error)) if error.contains("no nothing.here")));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The same over TCP, encrypted, to a "remote" server with its key; the
    /// wrong key gets nowhere.
    #[test]
    fn remote_calls_need_the_key() {
        let address = server::tests::start_test_remote("rpc-remote");
        let dir = std::env::temp_dir().join(format!("xh-rpc-remote-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("far.txt");
        std::fs::write(&file, "far away").unwrap();

        let connection = Connection::open(server::connect(&address).unwrap()).unwrap();
        assert_eq!(connection.call("files.read", &json!([file])).unwrap(), json!({ "Ok": "far away" }));
        let info: ServerInfo = serde_json::from_value(connection.call("server.info", &Value::Null).unwrap()).unwrap();
        assert_eq!(info.protocol, server::PROTOCOL);

        let HostAddress::Remote(key) = address else { unreachable!() };
        let wrong = crate::services::transport::ServerKey { secret: crate::services::transport::ServerKey::generate("x", "h", 1).unwrap().secret, ..key };
        assert!(server::connect(&HostAddress::Remote(wrong)).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}

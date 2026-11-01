//! A Language Server Protocol client: starts servers from the Marketplace
//! (see `services/language_servers.rs`), one per language and workspace root, and
//! shares them between editors.
//!
//! Messages are JSON-RPC over the server's stdin/stdout. A thread per
//! server reads replies (answering the requests we sent) and notifications
//! (diagnostics), and answers the server's own requests with nothing.
//! Until the server has answered `initialize`, everything we send waits.

use std::{
    collections::{HashMap, HashSet},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
    },
};

use futures::{
    StreamExt,
    channel::{mpsc, oneshot},
};
use gpui::{App, AppContext, Context, Entity, Global};
use serde_json::{Value, json};

use crate::services::language_servers;

pub type Reply = Result<Value, String>;

/// A running server.
pub struct Client {
    pub name: &'static str,
    stdin: Mutex<ChildStdin>,
    child: Mutex<Child>,
    next_id: AtomicI64,
    pending: Mutex<HashMap<i64, oneshot::Sender<Reply>>>,
    /// Messages waiting for `initialize` to finish (`None` once it has).
    queue: Mutex<Option<Vec<Value>>>,
    /// What the server said it can do.
    capabilities: Mutex<Value>,
}

impl Client {
    fn spawn(name: &'static str, binary: &Path, args: &[&str], root: &Path) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<Value>), String> {
        let mut child = crate::services::command(binary)
            .args(args)
            .current_dir(root)
            .env("PATH", language_servers::user_path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("Can't start {name}: {error}."))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let client = Arc::new(Self {
            name,
            stdin: Mutex::new(stdin),
            child: Mutex::new(child),
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            queue: Mutex::new(Some(Vec::new())),
            capabilities: Mutex::new(Value::Null),
        });
        let (notifications, receiver) = mpsc::unbounded();
        let reader = client.clone();
        std::thread::Builder::new()
            .name(format!("lsp-{name}"))
            .spawn(move || reader.read_loop(stdout, notifications))
            .map_err(|e| e.to_string())?;
        Ok((client, receiver))
    }

    fn read_loop(self: Arc<Self>, stdout: impl Read, notifications: mpsc::UnboundedSender<Value>) {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut length = None;
            loop {
                let mut header = String::new();
                match reader.read_line(&mut header) {
                    Ok(0) | Err(_) => return self.fail_pending(),
                    Ok(_) => {}
                }
                let header = header.trim_end();
                if header.is_empty() {
                    break;
                }
                if let Some(value) = header.strip_prefix("Content-Length:") {
                    length = value.trim().parse::<usize>().ok();
                }
            }
            let Some(length) = length else {
                continue;
            };
            let mut body = vec![0; length];
            if reader.read_exact(&mut body).is_err() {
                return self.fail_pending();
            }
            let Ok(message) = serde_json::from_slice::<Value>(&body) else {
                continue;
            };
            let id = message.get("id").cloned();
            match (message.get("method"), id) {
                // A reply to one of ours.
                (None, Some(id)) => {
                    let Some(id) = id.as_i64() else {
                        continue;
                    };
                    if let Some(sender) = self.pending.lock().unwrap().remove(&id) {
                        let reply = match message.get("error") {
                            Some(error) => Err(error.get("message").and_then(Value::as_str).unwrap_or("error").to_string()),
                            None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        sender.send(reply).ok();
                    }
                }
                // The server asks something (configuration, progress
                // tokens, capability registration): answer with nothing.
                (Some(method), Some(id)) => {
                    let result = if method == "workspace/configuration" {
                        let items = message["params"]["items"].as_array().map_or(0, Vec::len);
                        Value::Array(vec![Value::Null; items])
                    } else {
                        Value::Null
                    };
                    self.write(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
                }
                (Some(_), None) => {
                    notifications.unbounded_send(message).ok();
                }
                (None, None) => {}
            }
        }
    }

    fn fail_pending(&self) {
        for (_, sender) in self.pending.lock().unwrap().drain() {
            sender.send(Err(format!("{} stopped.", self.name))).ok();
        }
    }

    fn write(&self, message: &Value) {
        let body = message.to_string();
        let mut stdin = self.stdin.lock().unwrap();
        write!(stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).ok();
        stdin.flush().ok();
    }

    fn send(&self, message: Value) {
        let mut queue = self.queue.lock().unwrap();
        match queue.as_mut() {
            Some(waiting) => waiting.push(message),
            None => {
                drop(queue);
                self.write(&message);
            }
        }
    }

    pub fn notify(&self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    pub fn request(&self, method: &str, params: Value) -> oneshot::Receiver<Reply> {
        let (sender, receiver) = oneshot::channel();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.pending.lock().unwrap().insert(id, sender);
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        receiver
    }

    /// Whether the server takes edits one at a time (sync kind 2) rather
    /// than the whole text each time.
    pub fn incremental_sync(&self) -> bool {
        let capabilities = self.capabilities.lock().unwrap();
        let sync = &capabilities["textDocumentSync"];
        sync.as_u64().or_else(|| sync["change"].as_u64()) == Some(2)
    }

    /// Characters that should open completions (`.`, `::`'s `:`…).
    pub fn completion_triggers(&self) -> Vec<String> {
        self.capabilities.lock().unwrap()["completionProvider"]["triggerCharacters"]
            .as_array()
            .map(|chars| chars.iter().filter_map(|c| c.as_str().map(String::from)).collect())
            .unwrap_or_default()
    }

    /// Sends `initialize`, waits for it, then lets everything queued go.
    async fn initialize(self: Arc<Self>, root: PathBuf) -> Result<(), String> {
        let (sender, receiver) = oneshot::channel();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.pending.lock().unwrap().insert(id, sender);
        let uri = path_to_uri(&root);
        let name = root.file_name().map_or("root".to_string(), |n| n.to_string_lossy().into_owned());
        self.write(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "clientInfo": { "name": "xharness" },
                "rootUri": uri,
                "workspaceFolders": [{ "uri": uri, "name": name }],
                "capabilities": {
                    "textDocument": {
                        "synchronization": { "didSave": true, "dynamicRegistration": false },
                        "publishDiagnostics": { "relatedInformation": false },
                        "completion": {
                            "completionItem": { "snippetSupport": false, "documentationFormat": ["plaintext"] },
                            "contextSupport": true
                        },
                        "hover": { "contentFormat": ["plaintext", "markdown"] },
                        "definition": { "linkSupport": true }
                    },
                    "workspace": { "workspaceFolders": true, "configuration": true },
                    "window": { "workDoneProgress": false }
                }
            }
        }));
        let reply = receiver.await.map_err(|_| format!("{} stopped.", self.name))??;
        *self.capabilities.lock().unwrap() = reply.get("capabilities").cloned().unwrap_or(Value::Null);
        self.write(&json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }));
        let waiting = self.queue.lock().unwrap().take().unwrap_or_default();
        for message in waiting {
            self.write(&message);
        }
        Ok(())
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            child.kill().ok();
        }
    }
}

/// A problem a server found, in UTF-16 columns.
#[derive(Clone, Debug, PartialEq)]
pub struct Diagnostic {
    pub start: (usize, usize),
    pub end: (usize, usize),
    /// 1 error, 2 warning, 3 information, 4 hint.
    pub severity: u8,
    pub message: String,
}

/// The running servers and what they've reported, shared by every editor.
pub struct Lsp {
    servers: HashMap<(&'static str, PathBuf), Arc<Client>>,
    failed: HashSet<(&'static str, PathBuf)>,
    diagnostics: HashMap<PathBuf, (u64, Vec<Diagnostic>)>,
    next_version: u64,
    generation: u64,
}

struct GlobalLsp(Entity<Lsp>);

impl Global for GlobalLsp {}

/// What starts a server: binary, launcher, LSP language id, catalog entry.
pub type Found = (PathBuf, &'static language_servers::Launcher, &'static str, &'static language_servers::Spec);

/// An open document's link to its server.
pub struct Attached {
    pub client: Arc<Client>,
    pub language_id: &'static str,
}

impl Lsp {
    pub fn global(cx: &mut App) -> Entity<Lsp> {
        if let Some(global) = cx.try_global::<GlobalLsp>() {
            return global.0.clone();
        }
        let lsp = cx.new(|_| Lsp {
            servers: HashMap::new(),
            failed: HashSet::new(),
            diagnostics: HashMap::new(),
            next_version: 1,
            generation: 0,
        });
        cx.set_global(GlobalLsp(lsp.clone()));
        lsp
    }

    /// Bumped when servers are installed, so editors without one try again.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// After installing: servers that couldn't start may now.
    pub fn servers_changed(&mut self, cx: &mut Context<Self>) {
        self.failed.clear();
        self.generation += 1;
        cx.notify();
    }

    /// The server for `path`, started if it isn't yet, given what
    /// [`language_servers::launcher_for`] found (run that off the main
    /// thread: it can ask your shell for PATH).
    pub fn attach(&mut self, path: &Path, found: Found, cx: &mut Context<Self>) -> Option<Attached> {
        let (binary, launcher, language_id, spec) = found;
        let root = language_servers::root_for(path, launcher.markers);
        let key = (launcher.binary, root.clone());
        if let Some(client) = self.servers.get(&key) {
            return Some(Attached { client: client.clone(), language_id });
        }
        if self.failed.contains(&key) {
            return None;
        }
        let (client, mut notifications) = match Client::spawn(spec.name, &binary, launcher.args, &root) {
            Ok(started) => started,
            Err(error) => {
                eprintln!("xharness: {error}");
                self.failed.insert(key);
                return None;
            }
        };
        self.servers.insert(key.clone(), client.clone());
        let init = client.clone().initialize(root);
        cx.spawn(async move |this, cx| {
            if let Err(error) = init.await {
                eprintln!("xharness: {error}");
                this.update(cx, |this, _| {
                    this.servers.remove(&key);
                    this.failed.insert(key.clone());
                })
                .ok();
                return;
            }
            while let Some(message) = notifications.next().await {
                if this.update(cx, |this, cx| this.handle(message, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        Some(Attached { client, language_id })
    }

    fn handle(&mut self, message: Value, cx: &mut Context<Self>) {
        if message["method"] != "textDocument/publishDiagnostics" {
            return;
        }
        let params = &message["params"];
        let Some(path) = params["uri"].as_str().and_then(uri_to_path) else {
            return;
        };
        let position = |p: &Value| (p["line"].as_u64().unwrap_or(0) as usize, p["character"].as_u64().unwrap_or(0) as usize);
        let diagnostics = params["diagnostics"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|d| Diagnostic {
                        start: position(&d["range"]["start"]),
                        end: position(&d["range"]["end"]),
                        severity: d["severity"].as_u64().unwrap_or(1) as u8,
                        message: d["message"].as_str().unwrap_or_default().to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.next_version += 1;
        self.diagnostics.insert(path, (self.next_version, diagnostics));
        cx.notify();
    }

    /// What servers reported for `path`, and a number that changes when
    /// they report again.
    pub fn diagnostics(&self, path: &Path) -> Option<(u64, &[Diagnostic])> {
        self.diagnostics.get(path).map(|(version, items)| (*version, items.as_slice()))
    }
}

/// `file:///Users/me/a%20b.rs` for `/Users/me/a b.rs`.
pub fn path_to_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => uri.push(byte as char),
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    uri
}

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let encoded = uri.strip_prefix("file://")?;
    let mut bytes = Vec::with_capacity(encoded.len());
    let mut iter = encoded.bytes();
    while let Some(byte) = iter.next() {
        if byte == b'%' {
            let hex = [iter.next()?, iter.next()?];
            bytes.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            bytes.push(byte);
        }
    }
    Some(PathBuf::from(String::from_utf8(bytes).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real server (clangd, from Xcode's tools) answers and reports an
    /// error in a file we open.
    #[test]
    fn clangd_reports_an_error() {
        let Some(clangd) = language_servers::find_command("clangd") else {
            return;
        };
        let root = std::env::temp_dir().join(format!("xh-lsp-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let file = root.join("main.c");
        let text = "int main(void) { return missing; }\n";
        std::fs::write(&file, text).unwrap();

        let (client, mut notifications) = Client::spawn("clangd", &clangd, &[], &root).unwrap();
        futures::executor::block_on(async {
            client.clone().initialize(root.clone()).await.unwrap();
            client.notify(
                "textDocument/didOpen",
                json!({ "textDocument": { "uri": path_to_uri(&file), "languageId": "c", "version": 1, "text": text } }),
            );
            loop {
                let message = notifications.next().await.expect("clangd keeps running");
                if message["method"] == "textDocument/publishDiagnostics" {
                    let diagnostics = message["params"]["diagnostics"].as_array().unwrap();
                    assert!(
                        diagnostics.iter().any(|d| d["message"].as_str().unwrap().contains("missing")),
                        "{diagnostics:?}"
                    );
                    break;
                }
            }
            // Requests get replies too.
            let hover = client
                .request("textDocument/hover", json!({ "textDocument": { "uri": path_to_uri(&file) }, "position": { "line": 0, "character": 4 } }))
                .await
                .unwrap();
            assert!(hover.is_ok());
        });
        drop(client);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn uris_round_trip() {
        let path = Path::new("/Users/me/my project/main.rs");
        let uri = path_to_uri(path);
        assert_eq!(uri, "file:///Users/me/my%20project/main.rs");
        assert_eq!(uri_to_path(&uri).as_deref(), Some(path));
        assert_eq!(uri_to_path("file:///tmp/%C3%A9.txt").unwrap(), PathBuf::from("/tmp/é.txt"));
    }
}

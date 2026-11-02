//! Everything without a UI. Most of it is the server's work (see
//! `server.rs` and `rpc.rs`): shells, files, git (and its GitHub sign-in)
//! lino and SFTP connections run there, so they keep going when the app quits. Language
//! servers run in the app.

pub mod files;
pub mod git;
pub mod github;
#[cfg(feature = "gui")]
pub mod language_servers;
pub mod lino;
#[cfg(feature = "gui")]
pub mod lsp;
pub mod paths;
#[cfg(feature = "gui")]
pub mod projects;
pub mod rpc;
pub mod pty;
pub mod server;
pub mod sftp;
pub mod shell;
pub mod transport;

/// A command to run a console program (git, a language server). On Windows
/// it gets no console window: the app is a GUI program, so each one would
/// flash a window otherwise.
pub fn command(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    #[allow(unused_mut)]
    let mut command = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// Runs a call the app sent the server (see `rpc.rs`).
pub fn dispatch(method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
    server::dispatch(method, params.clone())
        .or_else(|| files::dispatch(method, params.clone()))
        .or_else(|| git::dispatch(method, params.clone()))
        .or_else(|| github::dispatch(method, params.clone()))
        .or_else(|| lino::dispatch(method, params.clone()))
        .or_else(|| sftp::dispatch(method, params))
        .unwrap_or_else(|| Err(format!("the server has no {method}")))
}

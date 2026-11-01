//! A shell on a pseudo terminal, the same way on every platform: alacritty's
//! PTY on macOS and Linux, ConPTY (through WezTerm's portable-pty) on
//! Windows. The session host reads the output on one thread and writes
//! input from another.

use std::{
    io::{self, Read, Write},
    path::PathBuf,
};

use alacritty_terminal::event::WindowSize;

pub type Reader = Box<dyn Read + Send>;
pub type Writer = Box<dyn Write + Send>;

/// The running shell. Dropping it ends the shell.
pub struct ShellPty {
    #[cfg(unix)]
    pty: alacritty_terminal::tty::Pty,
    #[cfg(windows)]
    master: std::sync::Arc<std::sync::Mutex<Option<Box<dyn portable_pty::MasterPty + Send>>>>,
    #[cfg(windows)]
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
}

impl ShellPty {
    /// Starts the shell in `folder` (home if it's missing). Returns the PTY,
    /// its output (blocking reads; EOF or an error once the shell is gone),
    /// and its input.
    #[cfg(unix)]
    pub fn spawn(folder: Option<PathBuf>, size: WindowSize) -> io::Result<(Self, Reader, Writer)> {
        let pty = crate::services::shell::spawn(folder, size)?;
        let output = pty.file().try_clone()?;
        // alacritty makes the PTY non-blocking for its own event loop; the
        // host reads it with a plain blocking thread instead.
        set_blocking(&output)?;
        let input = pty.file().try_clone()?;
        Ok((Self { pty }, Box::new(output), Box::new(input)))
    }

    #[cfg(unix)]
    pub fn resize(&mut self, size: WindowSize) {
        use alacritty_terminal::event::OnResize;
        self.pty.on_resize(size);
    }

    #[cfg(windows)]
    pub fn spawn(folder: Option<PathBuf>, size: WindowSize) -> io::Result<(Self, Reader, Writer)> {
        use portable_pty::{CommandBuilder, native_pty_system};
        use std::sync::{Arc, Mutex};

        let pair = native_pty_system().openpty(pty_size(size)).map_err(io::Error::other)?;
        let shell = crate::services::shell::windows_shell();
        let mut command = CommandBuilder::new(&shell.program);
        command.args(&shell.args);
        for (key, value) in &shell.env {
            command.env(key, value);
        }
        if let Some(folder) = folder.filter(|f| f.is_dir()).or_else(std::env::home_dir) {
            command.cwd(folder);
        }
        let mut child = pair.slave.spawn_command(command).map_err(io::Error::other)?;
        drop(pair.slave);
        let output = pair.master.try_clone_reader().map_err(io::Error::other)?;
        let input = pair.master.take_writer().map_err(io::Error::other)?;
        let killer = child.clone_killer();
        let master = Arc::new(Mutex::new(Some(pair.master)));
        // ConPTY's output only ends once the console is closed: close it
        // when the shell exits, so the session ends with it.
        let closing = master.clone();
        std::thread::spawn(move || {
            child.wait().ok();
            closing.lock().unwrap().take();
        });
        Ok((Self { master, killer }, output, input))
    }

    #[cfg(windows)]
    pub fn resize(&mut self, size: WindowSize) {
        if let Some(master) = self.master.lock().unwrap().as_ref() {
            master.resize(pty_size(size)).ok();
        }
    }
}

#[cfg(windows)]
impl Drop for ShellPty {
    fn drop(&mut self) {
        self.killer.kill().ok();
        self.master.lock().unwrap().take();
    }
}

#[cfg(windows)]
fn pty_size(size: WindowSize) -> portable_pty::PtySize {
    portable_pty::PtySize {
        rows: size.num_lines.max(1),
        cols: size.num_cols.max(1),
        pixel_width: size.cell_width.saturating_mul(size.num_cols),
        pixel_height: size.cell_height.saturating_mul(size.num_lines),
    }
}

#[cfg(unix)]
fn set_blocking(file: &std::fs::File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let fd = file.as_raw_fd();
    // SAFETY: fcntl on a file descriptor we own.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

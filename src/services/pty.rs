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
        let input = SharedWriter(Arc::new(Mutex::new(pair.master.take_writer().map_err(io::Error::other)?)));
        let output = AnswerCursorQuery {
            output,
            input: input.clone(),
            pending: Vec::new(),
            answered: false,
        };
        let killer = child.clone_killer();
        let master = Arc::new(Mutex::new(Some(pair.master)));
        // ConPTY's output only ends once the console is closed: close it
        // when the shell exits, so the session ends with it.
        let closing = master.clone();
        std::thread::spawn(move || {
            child.wait().ok();
            closing.lock().unwrap().take();
        });
        Ok((Self { master, killer }, Box::new(output), Box::new(input)))
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

/// The shell's input, shared with [`AnswerCursorQuery`].
#[cfg(any(windows, test))]
#[derive(Clone)]
struct SharedWriter(std::sync::Arc<std::sync::Mutex<Writer>>);

#[cfg(any(windows, test))]
impl Write for SharedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

/// The shell's output, with ConPTY's opening question answered.
///
/// portable-pty creates the console to inherit the terminal's cursor, so
/// before showing anything ConPTY asks where the cursor is (`ESC[6n`) and
/// waits for the answer: until then the shell stays blank. The session host
/// answers instead of the app, since no window may be attached yet (and a
/// window replaying output doesn't answer old questions): a new console's
/// cursor is at the top left. The question is taken out of the output, so
/// nothing answers it twice.
#[cfg(any(windows, test))]
struct AnswerCursorQuery {
    output: Reader,
    input: SharedWriter,
    /// Output held back while it could still be the start of the question.
    pending: Vec<u8>,
    answered: bool,
}

#[cfg(any(windows, test))]
impl Read for AnswerCursorQuery {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        const QUERY: &[u8] = b"\x1b[6n";
        while !self.answered {
            let mut chunk = [0u8; 4096];
            let read = self.output.read(&mut chunk)?;
            self.pending.extend_from_slice(&chunk[..read]);
            if self.pending.starts_with(QUERY) {
                self.input.write_all(b"\x1b[1;1R")?;
                self.input.flush()?;
                self.pending.drain(..QUERY.len());
                self.answered = true;
            } else if read == 0 || !QUERY.starts_with(&self.pending[..self.pending.len().min(QUERY.len())]) {
                // Some other start (a console that doesn't ask), or the end.
                self.answered = true;
            }
        }
        if self.pending.is_empty() {
            return self.output.read(buffer);
        }
        let count = buffer.len().min(self.pending.len());
        buffer[..count].copy_from_slice(&self.pending[..count]);
        self.pending.drain(..count);
        Ok(count)
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// Output arriving a few bytes at a time, like a pipe.
    struct Trickle(Vec<Vec<u8>>);

    impl Read for Trickle {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.0.is_empty() {
                return Ok(0);
            }
            let chunk = self.0.remove(0);
            buffer[..chunk.len()].copy_from_slice(&chunk);
            Ok(chunk.len())
        }
    }

    fn run(chunks: &[&[u8]]) -> (Vec<u8>, Vec<u8>) {
        let typed = Arc::new(Mutex::new(Vec::new()));
        let mut output = AnswerCursorQuery {
            output: Box::new(Trickle(chunks.iter().map(|c| c.to_vec()).collect())),
            input: SharedWriter(Arc::new(Mutex::new(Box::new(Typed(typed.clone())) as Writer))),
            pending: Vec::new(),
            answered: false,
        };
        let mut shown = Vec::new();
        output.read_to_end(&mut shown).unwrap();
        let typed = typed.lock().unwrap().clone();
        (shown, typed)
    }

    struct Typed(Arc<Mutex<Vec<u8>>>);

    impl Write for Typed {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn conpty_cursor_question_is_answered_once_and_hidden() {
        // Split across reads, as a pipe may deliver it.
        let (shown, typed) = run(&[b"\x1b[", b"6n\x1b[?25l", b"PS C:\\> "]);
        assert_eq!(typed, b"\x1b[1;1R");
        assert_eq!(shown, b"\x1b[?25lPS C:\\> ");
        // Later questions are the terminal's to answer.
        let (shown, typed) = run(&[b"\x1b[6n", b"a\x1b[6n"]);
        assert_eq!((shown.as_slice(), typed.as_slice()), (&b"a\x1b[6n"[..], &b"\x1b[1;1R"[..]));
    }

    #[test]
    fn output_without_the_question_passes_through() {
        assert_eq!(run(&[b"\x1b[?25h", b"hello"]), (b"\x1b[?25hhello".to_vec(), vec![]));
        assert_eq!(run(&[b"\x1b["]), (b"\x1b[".to_vec(), vec![]));
        assert_eq!(run(&[]), (vec![], vec![]));
    }
}

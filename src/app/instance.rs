//! One app at a time. Starting it again (the menu bar icon's "Open
//! XHarness", the Dock, a terminal) brings the running app's window to the
//! front and quits, instead of opening a second window on the same
//! projects.
//!
//! The running app holds a lock on `app.lock` in the data folder (released
//! when it exits, however it exits) and listens on `app.sock` there; a
//! second start that can't take the lock writes `show` to the socket.
//! A packaged app gets this from macOS too, but a bare binary doesn't.

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::{fd::AsRawFd, unix::net::{UnixListener, UnixStream}},
    time::Duration,
};

use futures::channel::mpsc;

pub enum Instance {
    /// This is the app. Each `()` is another start asking to be shown.
    First(mpsc::UnboundedReceiver<()>),
    /// Another app is running, and was asked to show itself.
    AlreadyRunning,
}

pub fn claim() -> Instance {
    let (sender, receiver) = mpsc::unbounded();
    let Some(dir) = crate::services::paths::data_dir() else {
        return Instance::First(receiver);
    };
    if fs::create_dir_all(&dir).is_err() {
        return Instance::First(receiver);
    }
    let Ok(lock) = fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("app.lock")) else {
        return Instance::First(receiver);
    };
    let socket = dir.join("app.sock");
    // SAFETY: flock on a file we just opened.
    let locked = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    if !locked {
        // It may be starting up and not listening yet: try for a moment.
        for _ in 0..20 {
            if let Ok(mut stream) = UnixStream::connect(&socket) {
                stream.write_all(b"show\n").ok();
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        return Instance::AlreadyRunning;
    }
    // Held until the process ends.
    std::mem::forget(lock);
    fs::remove_file(&socket).ok();
    if let Ok(listener) = UnixListener::bind(&socket) {
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                stream.set_read_timeout(Some(Duration::from_secs(1))).ok();
                let mut line = String::new();
                BufReader::new(stream).read_line(&mut line).ok();
                if line.trim() == "show" && sender.unbounded_send(()).is_err() {
                    break;
                }
            }
        });
    }
    Instance::First(receiver)
}

//! Where the app and the server keep their files.

use std::{
    path::PathBuf,
    process::Stdio,
    sync::OnceLock,
    time::{Duration, Instant},
};

/// The data folder: `~/Library/Application Support/xharness` on macOS,
/// `%APPDATA%\\xharness` on Windows, `$XDG_DATA_HOME/xharness` (or
/// `~/.local/share/xharness`) elsewhere.
pub fn data_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        return std::env::var_os("APPDATA").map(|data| PathBuf::from(data).join("xharness"));
    }
    let home = std::env::home_dir()?;
    if cfg!(target_os = "macos") {
        Some(home.join("Library/Application Support/xharness"))
    } else {
        let data = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"));
        Some(data.join("xharness"))
    }
}

/// Your shell's PATH (an app started from Finder has a bare one), so npm,
/// node and go are found where you installed them.
pub fn user_path() -> &'static str {
    static PATH: OnceLock<String> = OnceLock::new();
    PATH.get_or_init(|| {
        // Windows apps get the user's PATH as it is.
        if cfg!(windows) {
            return std::env::var("PATH").unwrap_or_default();
        }
        let fallback = || {
            let mut parts: Vec<String> = std::env::var("PATH").unwrap_or_default().split(':').map(String::from).collect();
            for extra in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"] {
                if !parts.iter().any(|p| p == extra) {
                    parts.push(extra.to_string());
                }
            }
            if let Some(home) = std::env::home_dir() {
                for extra in [".cargo/bin", "go/bin", ".volta/bin", ".local/bin"] {
                    parts.push(home.join(extra).display().to_string());
                }
            }
            parts.join(":")
        };
        // A login, interactive zsh sets PATH as your terminal does; give it
        // a few seconds.
        let child = crate::services::command("/bin/zsh")
            .args(["-ilc", "printf '__PATH__%s__PATH__' \"$PATH\""])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = child else {
            return fallback();
        };
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = child.try_wait() {
                let mut out = String::new();
                if let Some(mut stdout) = child.stdout.take() {
                    std::io::Read::read_to_string(&mut stdout, &mut out).ok();
                }
                if let Some(path) = out.split("__PATH__").nth(1).filter(|p| !p.is_empty()) {
                    return format!("{path}:{}", fallback());
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        child.kill().ok();
        fallback()
    })
}

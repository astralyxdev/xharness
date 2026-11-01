//! Starting shells: zsh with the app's own startup files instead of yours
//! (PowerShell with the app's own profile on Windows), on a fresh PTY.
//! Used by the session host, which owns every shell (see `pty.rs`).

#[cfg(unix)]
use std::collections::HashMap;
use std::{io, path::Path};
#[cfg(unix)]
use std::path::PathBuf;

#[cfg(unix)]
use alacritty_terminal::{
    event::WindowSize,
    tty::{self, Pty},
};

/// The app's own zsh setup, used instead of yours.
#[cfg(unix)]
const ZSHRC: &str = r#"# Written by xharness each time a terminal starts; edits are overwritten.
# Terminals in xharness don't load your ~/.zshrc, so prompts, themes and
# plugins from it (oh-my-zsh and the like) don't apply here.

HISTFILE="$ZDOTDIR/.zsh_history"
HISTSIZE=10000
SAVEHIST=10000
setopt HIST_IGNORE_DUPS INC_APPEND_HISTORY

# A quiet prompt: the folder, dimmed, and a chevron.
PROMPT='%F{8}%1~ ❯%f '
RPROMPT=''
# No reverse-video "%" when output doesn't end with a newline.
PROMPT_EOL_MARK=''

# Tell the app what's happening, with the standard prompt marks (OSC 133:
# D = the command finished, A = a prompt starts, C = a command starts) so
# keys go to the command bar at a prompt and straight to programs while
# they run. And the window title: the folder's full path at a prompt (the
# app remembers it, to start there again), the command while one runs.
autoload -Uz add-zsh-hook
_xharness_prompt() { print -Pn '\e]133;D\a\e]133;A\a\e]2;%/\a' }
_xharness_command() { print -rn -- $'\e]133;C\a\e]2;'"$1"$'\a' }
add-zsh-hook precmd _xharness_prompt
add-zsh-hook preexec _xharness_command

# The lino that ships with xharness (the one the Lino window watches) comes
# first, ahead of any other on PATH.
[[ -n $XHARNESS_LINO_BIN ]] && path=("$XHARNESS_LINO_BIN" $path)
"#;

/// The same for bash, where zsh isn't installed.
#[cfg(unix)]
const BASHRC: &str = r#"# Written by xharness each time a terminal starts; edits are overwritten.
# Terminals in xharness don't load your ~/.bashrc.
PS1='\[\e]133;A\a\]\[\e[90m\]\W ❯\[\e[0m\] '
PS0='\[\e]133;C\a\]'
PROMPT_COMMAND='printf "\e]133;D\a\e]2;%s\a" "$PWD"'
[ -n "$XHARNESS_LINO_BIN" ] && PATH="$XHARNESS_LINO_BIN:$PATH"
"#;

/// Starts a shell on a new PTY, in `folder` (home if it's missing or gone).
#[cfg(unix)]
pub fn spawn(folder: Option<PathBuf>, size: WindowSize) -> io::Result<Pty> {
    let (shell, mut env) = own_shell();
    env.extend([
        ("TERM".to_string(), "xterm-256color".to_string()),
        ("COLORTERM".to_string(), "truecolor".to_string()),
        ("TERM_PROGRAM".to_string(), "xharness".to_string()),
    ]);
    if let Some(dir) = crate::services::lino::Lino::bundled_dir() {
        env.insert("XHARNESS_LINO_BIN".to_string(), dir.display().to_string());
    }
    let options = tty::Options {
        shell: Some(shell),
        working_directory: folder.filter(|f| f.is_dir()).or_else(std::env::home_dir),
        drain_on_exit: false,
        env,
    };
    tty::new(&options, size, 0)
}

/// The shell to run, and environment for it.
///
/// zsh, pointed at the app's own startup files with `ZDOTDIR`, so your
/// `~/.zshrc` never loads. The system's files (`/etc/zprofile`, which sets
/// `PATH`) still run. Falls back to bash with its own rc file, then to a
/// plain `sh`.
#[cfg(unix)]
fn own_shell() -> (tty::Shell, HashMap<String, String>) {
    let mut env = HashMap::new();
    // The prompt's chevron needs a UTF-8 locale, which apps started from
    // Finder don't have.
    if std::env::var_os("LANG").is_none() {
        env.insert("LANG".to_string(), "en_US.UTF-8".to_string());
    }
    let dir = crate::services::paths::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("shell");

    if Path::new("/bin/zsh").exists() {
        let args = match write_config(&dir, ".zshrc", ZSHRC) {
            Ok(()) => {
                env.insert("ZDOTDIR".to_string(), dir.display().to_string());
                vec!["-l".to_string()]
            }
            // Without our files, at least skip yours.
            Err(_) => vec!["-f".to_string()],
        };
        return (tty::Shell::new("/bin/zsh".to_string(), args), env);
    }
    if write_config(&dir, "bashrc", BASHRC).is_ok() {
        let rc = dir.join("bashrc").display().to_string();
        let args = ["--noprofile", "--rcfile", &rc, "-i"].map(String::from).to_vec();
        return (tty::Shell::new("/bin/bash".to_string(), args), env);
    }
    (tty::Shell::new("/bin/sh".to_string(), Vec::new()), env)
}

/// Writes a startup file, only when it changed (shells start together when
/// a project opens).
fn write_config(dir: &Path, name: &str, contents: &str) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(name);
    if std::fs::read_to_string(&path).ok().as_deref() != Some(contents) {
        std::fs::write(&path, contents)?;
    }
    Ok(())
}

/// The app's own PowerShell profile, used instead of yours: the same prompt
/// marks (OSC 133) and folder-as-title the zsh setup sends, so the canvas
/// knows when a command runs and where the shell is.
#[cfg(windows)]
const PROFILE: &str = r#"# Written by xharness each time a terminal starts; edits are overwritten.
# Terminals in xharness don't load your PowerShell profile.
if ($env:XHARNESS_LINO_BIN) { $env:PATH = "$env:XHARNESS_LINO_BIN;$env:PATH" }
function global:prompt {
    $e = [char]27; $b = [char]7
    $path = $executionContext.SessionState.Path.CurrentLocation.ProviderPath
    $name = Split-Path -Leaf $path
    if (-not $name) { $name = $path }
    "$e]133;D$b$e]133;A$b$e]2;$path$b$e[90m$name >$e[0m "
}
if (Get-Command Set-PSReadLineKeyHandler -ErrorAction SilentlyContinue) {
    Set-PSReadLineKeyHandler -Key Enter -ScriptBlock {
        [Console]::Write("$([char]27)]133;C$([char]7)")
        [Microsoft.PowerShell.PSConsoleReadLine]::AcceptLine()
    }
}
"#;

/// What to run for a terminal on Windows.
#[cfg(windows)]
pub struct WindowsShell {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// PowerShell 7 (`pwsh`) if it's installed, else Windows PowerShell, with
/// the app's profile dot-sourced (so its prompt is the session's).
#[cfg(windows)]
pub fn windows_shell() -> WindowsShell {
    let program = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|dir| dir.join("pwsh.exe"))
        .find(|path| path.is_file())
        .map_or("powershell.exe".to_string(), |path| path.display().to_string());
    let mut env = vec![
        ("TERM".to_string(), "xterm-256color".to_string()),
        ("COLORTERM".to_string(), "truecolor".to_string()),
        ("TERM_PROGRAM".to_string(), "xharness".to_string()),
    ];
    if let Some(dir) = crate::services::lino::Lino::bundled_dir() {
        env.push(("XHARNESS_LINO_BIN".to_string(), dir.display().to_string()));
    }
    let dir = crate::services::paths::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("shell");
    let mut args = vec!["-NoLogo".to_string()];
    if write_config(&dir, "profile.ps1", PROFILE).is_ok() {
        let profile = dir.join("profile.ps1").display().to_string().replace('\'', "''");
        args.extend([
            "-NoProfile".to_string(),
            "-NoExit".to_string(),
            "-ExecutionPolicy".to_string(),
            "Bypass".to_string(),
            "-Command".to_string(),
            format!(". '{profile}'"),
        ]);
    }
    WindowsShell { program, args, env }
}

//! The language servers the Marketplace offers: what each is, how to
//! install it, and how to start it for a file.
//!
//! Servers install into the data directory (`language-servers/<id>/`):
//! release binaries come from GitHub (with the system's `curl`, `gunzip`,
//! `tar`, `unzip`), npm packages with your `npm`, Go tools with `go
//! install`. One that's already on your system (clangd and sourcekit-lsp
//! come with Xcode's tools) is used as is.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::OnceLock,
};

/// How a server gets installed.
pub enum Source {
    /// A gzipped binary in each GitHub release, by architecture.
    GithubGz { repo: &'static str, asset: fn(&str) -> String },
    /// A plain binary in each GitHub release.
    GithubBinary { repo: &'static str, asset: &'static str },
    Npm { packages: &'static [&'static str] },
    Go { module: &'static str },
    /// Comes with the system (Xcode's tools); nothing to install.
    System,
}

/// One server, started for files of `languages`.
pub struct Launcher {
    /// What to run: a path in the install folder, or a command on PATH.
    pub binary: &'static str,
    pub args: &'static [&'static str],
    /// Languages it serves: the editor's name for each (syntect's) and the
    /// LSP id.
    pub languages: &'static [(&'static str, &'static str)],
    /// Files that mark a project's top folder (the workspace root).
    pub markers: &'static [&'static str],
}

pub struct Spec {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    /// What the card lists, e.g. "TypeScript · JavaScript".
    pub languages: &'static str,
    pub source: Source,
    /// Commands to look for on the system before installing.
    pub system: &'static [&'static str],
    pub launchers: &'static [Launcher],
}

const JS: &[(&str, &str)] = &[
    ("TypeScript", "typescript"),
    ("TypeScriptReact", "typescriptreact"),
    ("JavaScript", "javascript"),
    ("JavaScript (Babel)", "javascriptreact"),
    ("JSX", "javascriptreact"),
];

pub const CATALOG: &[Spec] = &[
    Spec {
        id: "rust-analyzer",
        name: "rust-analyzer",
        description: "The Rust language server: errors as you type, completions, go to definition.",
        languages: "Rust",
        source: Source::GithubGz {
            repo: "rust-lang/rust-analyzer",
            asset: |arch| format!("rust-analyzer-{arch}-apple-darwin.gz"),
        },
        system: &["rust-analyzer"],
        launchers: &[Launcher {
            binary: "rust-analyzer",
            args: &[],
            languages: &[("Rust", "rust")],
            markers: &["Cargo.toml"],
        }],
    },
    Spec {
        id: "typescript",
        name: "TypeScript Language Server",
        description: "TypeScript and JavaScript, through the TypeScript compiler's tsserver.",
        languages: "TypeScript · JavaScript",
        source: Source::Npm { packages: &["typescript-language-server", "typescript"] },
        system: &["typescript-language-server"],
        launchers: &[Launcher {
            binary: "node_modules/.bin/typescript-language-server",
            args: &["--stdio"],
            languages: JS,
            markers: &["tsconfig.json", "jsconfig.json", "package.json"],
        }],
    },
    Spec {
        id: "pyright",
        name: "Pyright",
        description: "Python type checking and completions, from Microsoft.",
        languages: "Python",
        source: Source::Npm { packages: &["pyright"] },
        system: &["pyright-langserver"],
        launchers: &[Launcher {
            binary: "node_modules/.bin/pyright-langserver",
            args: &["--stdio"],
            languages: &[("Python", "python")],
            markers: &["pyproject.toml", "setup.py", "requirements.txt", "pyrightconfig.json"],
        }],
    },
    Spec {
        id: "gopls",
        name: "gopls",
        description: "The Go team's language server.",
        languages: "Go",
        source: Source::Go { module: "golang.org/x/tools/gopls@latest" },
        system: &["gopls"],
        launchers: &[Launcher {
            binary: "gopls",
            args: &[],
            languages: &[("Go", "go")],
            markers: &["go.work", "go.mod"],
        }],
    },
    Spec {
        id: "clangd",
        name: "clangd",
        description: "C, C++ and Objective-C, from LLVM. Comes with Xcode's command line tools.",
        languages: "C · C++ · Objective-C",
        source: Source::System,
        system: &["clangd"],
        launchers: &[Launcher {
            binary: "clangd",
            args: &[],
            languages: &[("C", "c"), ("C++", "cpp"), ("Objective-C", "objective-c"), ("Objective-C++", "objective-cpp")],
            markers: &["compile_commands.json", "compile_flags.txt", ".clangd", "CMakeLists.txt", "Makefile"],
        }],
    },
    Spec {
        id: "sourcekit-lsp",
        name: "SourceKit-LSP",
        description: "Swift, from Apple. Comes with Xcode's command line tools.",
        languages: "Swift",
        source: Source::System,
        system: &["sourcekit-lsp"],
        launchers: &[Launcher {
            binary: "sourcekit-lsp",
            args: &[],
            languages: &[("Swift", "swift")],
            markers: &["Package.swift"],
        }],
    },
    Spec {
        id: "web",
        name: "JSON, CSS & HTML",
        description: "VS Code's language servers for JSON, CSS and HTML.",
        languages: "JSON · CSS · HTML",
        source: Source::Npm { packages: &["vscode-langservers-extracted"] },
        system: &[],
        launchers: &[
            Launcher {
                binary: "node_modules/.bin/vscode-json-language-server",
                args: &["--stdio"],
                languages: &[("JSON", "json")],
                markers: &["package.json"],
            },
            Launcher {
                binary: "node_modules/.bin/vscode-css-language-server",
                args: &["--stdio"],
                languages: &[("CSS", "css"), ("SCSS", "scss"), ("Less", "less")],
                markers: &["package.json"],
            },
            Launcher {
                binary: "node_modules/.bin/vscode-html-language-server",
                args: &["--stdio"],
                languages: &[("HTML", "html")],
                markers: &["package.json"],
            },
        ],
    },
    Spec {
        id: "yaml",
        name: "YAML Language Server",
        description: "YAML with schemas (GitHub workflows, Kubernetes, Compose…), from Red Hat.",
        languages: "YAML",
        source: Source::Npm { packages: &["yaml-language-server"] },
        system: &["yaml-language-server"],
        launchers: &[Launcher {
            binary: "node_modules/.bin/yaml-language-server",
            args: &["--stdio"],
            languages: &[("YAML", "yaml")],
            markers: &[],
        }],
    },
    Spec {
        id: "bash",
        name: "Bash Language Server",
        description: "Shell scripts, with ShellCheck's warnings when it's installed.",
        languages: "Bash · Shell",
        source: Source::Npm { packages: &["bash-language-server"] },
        system: &["bash-language-server"],
        launchers: &[Launcher {
            binary: "node_modules/.bin/bash-language-server",
            args: &["start"],
            languages: &[("Bourne Again Shell (bash)", "shellscript"), ("Shell-Unix-Generic", "shellscript")],
            markers: &[],
        }],
    },
    Spec {
        id: "taplo",
        name: "Taplo",
        description: "TOML: validation and completions for Cargo.toml, pyproject.toml and more.",
        languages: "TOML",
        source: Source::GithubGz {
            repo: "tamasfe/taplo",
            asset: |arch| format!("taplo-darwin-{}.gz", if arch == "aarch64" { "aarch64" } else { "x86_64" }),
        },
        system: &["taplo"],
        launchers: &[Launcher {
            binary: "taplo",
            args: &["lsp", "stdio"],
            languages: &[("TOML", "toml")],
            markers: &[],
        }],
    },
    Spec {
        id: "marksman",
        name: "Marksman",
        description: "Markdown: links between notes, headings, and references.",
        languages: "Markdown",
        source: Source::GithubBinary { repo: "artempyanykh/marksman", asset: "marksman-macos" },
        system: &["marksman"],
        launchers: &[Launcher {
            binary: "marksman",
            args: &["server"],
            languages: &[("Markdown", "markdown")],
            markers: &[".marksman.toml"],
        }],
    },
];

/// Where a server is installed.
pub fn install_dir(id: &str) -> Option<PathBuf> {
    crate::services::projects::data_dir().map(|dir| dir.join("language-servers").join(id))
}

const DONE_MARKER: &str = ".installed";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Installed {
    /// In xharness's folder.
    Here,
    /// Found on the system, at this path.
    System(PathBuf),
    No,
}

pub fn installed(spec: &Spec) -> Installed {
    if install_dir(spec.id).is_some_and(|dir| dir.join(DONE_MARKER).exists()) {
        return Installed::Here;
    }
    spec.system
        .iter()
        .find_map(|command| find_command(command))
        .map_or(Installed::No, Installed::System)
}

/// The command to start a server for a file in `language`: the binary,
/// its arguments, the LSP language id, and the launcher.
pub fn launcher_for(language: &str) -> Option<(PathBuf, &'static Launcher, &'static str, &'static Spec)> {
    for spec in CATALOG {
        for launcher in spec.launchers {
            let Some(&(_, id)) = launcher.languages.iter().find(|(name, _)| *name == language) else {
                continue;
            };
            let binary = match installed(spec) {
                Installed::Here => windows_binary(install_dir(spec.id)?.join(launcher.binary)),
                Installed::System(path) if launcher.binary.contains('/') => path,
                Installed::System(_) => find_command(launcher.binary)?,
                Installed::No => continue,
            };
            return Some((binary, launcher, id, spec));
        }
    }
    None
}

/// On Windows npm's shims are `.cmd` files, Go's binaries `.exe`.
fn windows_binary(path: PathBuf) -> PathBuf {
    if !cfg!(windows) || path.exists() {
        return path;
    }
    ["cmd", "exe"]
        .iter()
        .map(|ext| path.with_extension(ext))
        .find(|candidate| candidate.exists())
        .unwrap_or(path)
}

/// The workspace root for `file`: the nearest folder above it with one of
/// `markers`, else the repository's top folder, else the file's folder.
pub fn root_for(file: &Path, markers: &[&str]) -> PathBuf {
    let dir = file.parent().unwrap_or(file);
    let found = |names: &[&str]| dir.ancestors().find(|d| names.iter().any(|m| d.join(m).exists())).map(Path::to_path_buf);
    found(markers).or_else(|| found(&[".git"])).unwrap_or_else(|| dir.to_path_buf())
}

pub use crate::services::paths::user_path;

/// `command` on your PATH (or from Xcode's tools). Remembered: looking
/// can mean asking `xcrun`.
pub fn find_command(command: &str) -> Option<PathBuf> {
    static FOUND: OnceLock<std::sync::Mutex<std::collections::HashMap<String, Option<PathBuf>>>> = OnceLock::new();
    let cache = FOUND.get_or_init(Default::default);
    if let Some(found) = cache.lock().unwrap().get(command) {
        return found.clone();
    }
    let found = look_for_command(command);
    cache.lock().unwrap().insert(command.to_string(), found.clone());
    found
}

fn look_for_command(command: &str) -> Option<PathBuf> {
    // Windows runs `npm.cmd`, `gopls.exe`…
    let names: Vec<String> = if cfg!(windows) {
        ["", ".exe", ".cmd", ".bat"].iter().map(|ext| format!("{command}{ext}")).collect()
    } else {
        vec![command.to_string()]
    };
    let found = std::env::split_paths(user_path())
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)).collect::<Vec<_>>())
        .find(|path| path.is_file());
    found.or_else(|| {
        let out = crate::services::command("xcrun").args(["--find", command]).stderr(Stdio::null()).output().ok()?;
        let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        (out.status.success() && path.is_file()).then_some(path)
    })
}

fn run(program: &str, args: &[&str], dir: &Path) -> Result<(), String> {
    let program_path = find_command(program).unwrap_or_else(|| PathBuf::from(program));
    let output = crate::services::command(&program_path)
        .args(args)
        .current_dir(dir)
        .env("PATH", user_path())
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("Can't run {program}: {error}."))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let last = stderr.lines().filter(|l| !l.trim().is_empty()).last().unwrap_or("it failed");
        Err(format!("{program}: {}", last.trim()))
    }
}

/// Installs `spec` (blocking: run it in the background).
pub fn install(spec: &Spec) -> Result<(), String> {
    let dir = install_dir(spec.id).ok_or("No data folder to install into.")?;
    if dir.exists() {
        fs::remove_dir_all(&dir).map_err(|e| format!("Can't clear {}: {e}.", dir.display()))?;
    }
    fs::create_dir_all(&dir).map_err(|e| format!("Can't create {}: {e}.", dir.display()))?;
    let arch = if cfg!(target_arch = "aarch64") { "aarch64" } else { "x86_64" };
    let result = match &spec.source {
        Source::System => Err("This one comes with Xcode's command line tools: run `xcode-select --install`.".to_string()),
        Source::GithubGz { .. } | Source::GithubBinary { .. } if cfg!(windows) => Err(
            "This one isn't available for Windows here yet: install it yourself and put it on PATH.".to_string(),
        ),
        Source::GithubGz { repo, asset } => {
            let url = format!("https://github.com/{repo}/releases/latest/download/{}", asset(arch));
            let name = spec.launchers[0].binary;
            let archive = format!("{name}.gz");
            run("curl", &["-fsSL", "-o", &archive, &url], &dir)
                .and_then(|_| run("gunzip", &["-f", &archive], &dir))
                .and_then(|_| make_executable(&dir.join(name)))
        }
        Source::GithubBinary { repo, asset } => {
            let url = format!("https://github.com/{repo}/releases/latest/download/{asset}");
            let name = spec.launchers[0].binary;
            run("curl", &["-fsSL", "-o", name, &url], &dir).and_then(|_| make_executable(&dir.join(name)))
        }
        Source::Npm { packages } => {
            if find_command("npm").is_none() {
                Err("This one needs npm (Node.js): install it from nodejs.org or with Homebrew.".to_string())
            } else {
                let mut args = vec!["install", "--no-audit", "--no-fund", "--prefix", "."];
                args.extend(packages.iter());
                run("npm", &args, &dir)
            }
        }
        Source::Go { module } => {
            if find_command("go").is_none() {
                Err("This one needs Go: install it from go.dev or with Homebrew.".to_string())
            } else {
                let gobin = dir.display().to_string();
                let program = find_command("go").unwrap_or_else(|| PathBuf::from("go"));
                crate::services::command(program)
                    .args(["install", module])
                    .current_dir(&dir)
                    .env("GOBIN", &gobin)
                    .env("PATH", user_path())
                    .output()
                    .map_err(|e| format!("Can't run go: {e}."))
                    .and_then(|out| {
                        if out.status.success() {
                            Ok(())
                        } else {
                            Err(format!("go: {}", String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("failed")))
                        }
                    })
            }
        }
    };
    match result {
        Ok(()) => fs::write(dir.join(DONE_MARKER), "").map_err(|e| e.to_string()),
        Err(error) => {
            fs::remove_dir_all(&dir).ok();
            Err(error)
        }
    }
}

pub fn uninstall(spec: &Spec) -> Result<(), String> {
    match install_dir(spec.id) {
        Some(dir) if dir.exists() => fs::remove_dir_all(&dir).map_err(|e| e.to_string()),
        _ => Ok(()),
    }
}

fn make_executable(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_maps_to_one_server() {
        let mut seen = std::collections::HashSet::new();
        for spec in CATALOG {
            for launcher in spec.launchers {
                for (name, _) in launcher.languages {
                    assert!(seen.insert(*name), "{name} has two servers");
                }
            }
        }
        assert!(seen.contains("Rust") && seen.contains("TypeScriptReact"));
    }

    #[test]
    fn roots_come_from_markers() {
        let root = std::env::temp_dir().join(format!("xh-roots-{}", std::process::id()));
        fs::remove_dir_all(&root).ok();
        fs::create_dir_all(root.join("crate/src")).unwrap();
        fs::write(root.join("crate/Cargo.toml"), "").unwrap();
        assert_eq!(root_for(&root.join("crate/src/main.rs"), &["Cargo.toml"]), root.join("crate"));
        assert_eq!(root_for(&root.join("crate/src/main.rs"), &["go.mod"]), root.join("crate/src"), "no marker: the file's folder");
        fs::remove_dir_all(&root).ok();
    }
}

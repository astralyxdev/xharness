//! Gets lino and builds it for the platform xharness is being built for, so
//! the app can bundle it (see src/services/lino.rs):
//!
//! 1. Clones lino's repository at the pinned version (below) into the build
//!    folder, once, and checks it's the pinned commit.
//! 2. Builds `lino` and `lino-core` with Go for the target's OS and
//!    architecture (`GOOS`/`GOARCH`, no cgo), so cross builds bundle the
//!    right binaries.
//!
//! Needs git and Go (the version lino's go.mod asks for). Settings:
//!
//! - `XHARNESS_LINO_SRC=/path/to/lino` builds that checkout instead (when
//!   working on lino itself).
//! - `XHARNESS_LINO_REPOSITORY=<url>` clones from somewhere else (a mirror).
//! - `XHARNESS_SKIP_LINO=1` builds without lino; xharness then uses one on
//!   PATH.
//!
//! Windows builds go without: lino is Unix-only.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

/// Where lino comes from, and the version bundled: a tag, and the commit it
/// must be (a tag can move; the commit can't).
const LINO_REPOSITORY: &str = "https://github.com/astralyxdev/lino.git";
const LINO_TAG: &str = "v0.2.0";
const LINO_COMMIT: &str = "46f217d52b2981682591aefd9444734c14760c03";

fn main() {
    println!("cargo::rustc-check-cfg=cfg(lino_bundled)");
    println!("cargo::rerun-if-changed=build.rs");
    for setting in ["XHARNESS_SKIP_LINO", "XHARNESS_LINO_SRC", "XHARNESS_LINO_REPOSITORY"] {
        println!("cargo::rerun-if-env-changed={setting}");
    }

    if env::var_os("XHARNESS_SKIP_LINO").is_some() {
        println!("cargo::warning=XHARNESS_SKIP_LINO is set: building without the bundled lino");
        return;
    }
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    // lino uses flock and signals; its README says "Windows via WSL".
    if target_os == "windows" {
        println!("cargo::warning=lino doesn't support Windows: building without the bundled lino");
        return;
    }
    let goos = match target_os.as_str() {
        "macos" => "darwin",
        other => other,
    };
    let goarch = match target_arch.as_str() {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        "x86" => "386",
        other => other,
    };

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let (source, version, commit) = match env::var_os("XHARNESS_LINO_SRC") {
        Some(dir) => local_source(PathBuf::from(dir)),
        None => (
            clone(&out_dir.join("lino-src")),
            LINO_TAG.to_string(),
            LINO_COMMIT[..7].to_string(),
        ),
    };

    let out = out_dir.join("lino");
    fs::create_dir_all(&out).unwrap();
    let ldflags = format!("-s -w -X main.version={version} -X main.commit={commit}");
    for command in ["lino-core", "lino"] {
        let binary = out.join(command);
        let status = Command::new(go())
            .args(["build", "-trimpath", "-ldflags", &ldflags, "-o"])
            .arg(&binary)
            .arg(format!("./cmd/{command}"))
            .current_dir(&source)
            .env("CGO_ENABLED", "0")
            .env("GOOS", goos)
            .env("GOARCH", goarch)
            .status()
            .unwrap_or_else(|error| {
                panic!("\n\nCan't run Go to build lino ({error}). Install Go (go.dev, or `brew install go`), or set XHARNESS_SKIP_LINO=1.\n")
            });
        assert!(status.success(), "building {command} for {goos}/{goarch} failed");
    }
    println!("cargo::rustc-cfg=lino_bundled");
    println!("cargo::rustc-env=LINO_DIR={}", out.display());
    println!("cargo::rustc-env=LINO_EXE=");
    println!("cargo::rustc-env=LINO_VERSION={version}");
    println!("cargo::rustc-env=LINO_COMMIT={commit}");
}

/// lino at the pinned commit, in `dir`: cloned the first time (just that
/// commit), kept after that.
fn clone(dir: &Path) -> PathBuf {
    if dir.join(".git").exists() && git(dir, &["rev-parse", "HEAD"]).as_deref() == Some(LINO_COMMIT) {
        return dir.to_path_buf();
    }
    let repository = env::var("XHARNESS_LINO_REPOSITORY").unwrap_or_else(|_| LINO_REPOSITORY.to_string());
    let partial = dir.with_extension("partial");
    fs::remove_dir_all(&partial).ok();
    let fail = |what: &str| -> ! {
        panic!(
            "\n\nCan't get lino {LINO_TAG} from {repository}: {what}.\n\
             Check the network (git needs to reach it), or build a local checkout with \
             XHARNESS_LINO_SRC=/path/to/lino, or build without lino with XHARNESS_SKIP_LINO=1.\n"
        )
    };
    let cloned = git_command()
        .args(["clone", "--quiet", "--depth", "1", "--branch", LINO_TAG])
        .args(["--config", "advice.detachedHead=false", &repository])
        .arg(&partial)
        .status()
        .unwrap_or_else(|error| fail(&format!("can't run git ({error})")));
    if !cloned.success() {
        fail("git clone failed");
    }
    if git(&partial, &["rev-parse", "HEAD"]).as_deref() != Some(LINO_COMMIT) {
        // The tag moved: fetch the pinned commit itself.
        println!("cargo::warning=lino's {LINO_TAG} tag isn't commit {LINO_COMMIT} any more; building that commit");
        let fetched = git(&partial, &["fetch", "--quiet", "--depth", "1", "origin", LINO_COMMIT]).is_some()
            && git(&partial, &["checkout", "--quiet", "--detach", "FETCH_HEAD"]).is_some();
        if !fetched || git(&partial, &["rev-parse", "HEAD"]).as_deref() != Some(LINO_COMMIT) {
            fail(&format!("commit {LINO_COMMIT} isn't there"));
        }
    }
    fs::remove_dir_all(dir).ok();
    fs::rename(&partial, dir).unwrap_or_else(|error| fail(&format!("can't move the clone into place ({error})")));
    dir.to_path_buf()
}

/// A lino checkout of your own, with its version from git.
fn local_source(dir: PathBuf) -> (PathBuf, String, String) {
    assert!(dir.join("go.mod").is_file(), "XHARNESS_LINO_SRC ({}) isn't a lino checkout", dir.display());
    for path in ["cmd", "internal", "go.mod", "go.sum", ".git/HEAD"] {
        println!("cargo::rerun-if-changed={}", dir.join(path).display());
    }
    let version = git(&dir, &["describe", "--tags", "--always", "--dirty"]).unwrap_or_else(|| "dev".into());
    let commit = git(&dir, &["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "none".into());
    (dir, version, commit)
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = git_command().args(args).current_dir(dir).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// git, minding only the folder it runs in (not a repository cargo was
/// started from, as in a git hook).
fn git_command() -> Command {
    let mut command = Command::new("git");
    for variable in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_OBJECT_DIRECTORY", "GIT_COMMON_DIR"] {
        command.env_remove(variable);
    }
    command
}

/// `go` on PATH, or where Homebrew and the installer put it (cargo may run
/// without your shell's PATH).
fn go() -> PathBuf {
    let candidates = ["/opt/homebrew/bin/go", "/usr/local/go/bin/go", "/usr/local/bin/go"];
    env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| env::split_paths(&paths).collect::<Vec<_>>())
        .map(|dir| dir.join("go"))
        .chain(candidates.iter().map(PathBuf::from))
        .find(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("go"))
}

//! Git, through the `git` command line: status, diffs, history with its
//! branch graph, branches, commits, and syncing with the remote.
//!
//! Every call blocks while git runs; call them off the main thread. Calls
//! never prompt (`GIT_TERMINAL_PROMPT=0`), so pushing to a remote that
//! needs a password fails with git's message instead of hanging; macOS's
//! keychain helper usually has the credentials.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::services::rpc::{handle_call, route};

use std::{
    path::{Path, PathBuf},
    process::Stdio,
};

/// Commits shown in History.
pub const HISTORY_LIMIT: usize = 500;
/// Longer diffs are cut off.
const DIFF_LIMIT: usize = 20_000;

// Routed: these run in the server (see services/rpc.rs) --------------------------

pub fn discover(folder: &Path) -> Option<PathBuf> {
    route("git.discover", (folder,), || discover_here(folder))
}

pub fn init(folder: &Path) -> Result<(), String> {
    route("git.init", (folder,), || init_here(folder))
}

pub fn status(root: &Path) -> Result<Status, String> {
    route("git.status", (root,), || status_here(root))
}

pub fn diff_change(root: &Path, change: &Change, has_head: bool) -> Result<Diff, String> {
    route("git.diff_change", (root, change, has_head), || diff_change_here(root, change, has_head))
}

pub fn diff_in_commit(root: &Path, hash: &str, change: &Change) -> Result<Diff, String> {
    route("git.diff_in_commit", (root, hash, change), || diff_in_commit_here(root, hash, change))
}

pub fn commit_files(root: &Path, hash: &str) -> Result<Vec<Change>, String> {
    route("git.commit_files", (root, hash), || commit_files_here(root, hash))
}

pub fn history(root: &Path) -> Result<Vec<Commit>, String> {
    route("git.history", (root,), || history_here(root))
}

pub fn branches(root: &Path) -> Result<Vec<Branch>, String> {
    route("git.branches", (root,), || branches_here(root))
}

pub fn switch(root: &Path, branch: &Branch) -> Result<(), String> {
    route("git.switch", (root, branch), || switch_here(root, branch))
}

pub fn create_branch(root: &Path, name: &str) -> Result<(), String> {
    route("git.create_branch", (root, name), || create_branch_here(root, name))
}

pub fn remotes(root: &Path) -> Vec<String> {
    route("git.remotes", (root,), || remotes_here(root))
}

/// The remotes, with their URLs.
pub fn remote_list(root: &Path) -> Result<Vec<Remote>, String> {
    route("git.remote_list", (root,), || remote_list_here(root))
}

pub fn add_remote(root: &Path, name: &str, url: &str) -> Result<(), String> {
    route("git.add_remote", (root, name, url), || add_remote_here(root, name, url))
}

/// Renames remote `name` to `new_name` (when they differ) and sets its URL.
pub fn edit_remote(root: &Path, name: &str, new_name: &str, url: &str) -> Result<(), String> {
    route("git.edit_remote", (root, name, new_name, url), || edit_remote_here(root, name, new_name, url))
}

pub fn remove_remote(root: &Path, name: &str) -> Result<(), String> {
    route("git.remove_remote", (root, name), || remove_remote_here(root, name))
}

/// Whether git can read the repository at `url` (with the credentials it
/// has): its branches, or why not.
pub fn check_remote(root: &Path, url: &str) -> Result<usize, String> {
    route("git.check_remote", (root, url), || check_remote_here(root, url))
}

pub fn fetch(root: &Path) -> Result<(), String> {
    route("git.fetch", (root,), || fetch_here(root))
}

pub fn pull(root: &Path) -> Result<(), String> {
    route("git.pull", (root,), || pull_here(root))
}

pub fn push(root: &Path, status: &Status) -> Result<(), String> {
    route("git.push", (root, status), || push_here(root, status))
}

/// Pushes the branch to `remote` and tracks it there.
pub fn publish_to(root: &Path, remote: &str) -> Result<(), String> {
    route("git.publish_to", (root, remote), || publish_to_here(root, remote))
}

pub fn commit(root: &Path, summary: &str, description: &str, paths: Option<&[String]>) -> Result<(), String> {
    route("git.commit", (root, summary, description, paths), || commit_here(root, summary, description, paths))
}

pub fn discard(root: &Path, change: &Change) -> Result<(), String> {
    route("git.discard", (root, change), || discard_here(root, change))
}

/// The server runs these.
pub(crate) fn dispatch(method: &str, params: Value) -> Option<Result<Value, String>> {
    Some(match method {
        "git.discover" => handle_call(params, |(folder,): (PathBuf,)| discover(&folder)),
        "git.init" => handle_call(params, |(folder,): (PathBuf,)| init(&folder)),
        "git.status" => handle_call(params, |(root,): (PathBuf,)| status(&root)),
        "git.diff_change" => handle_call(params, |(root, change, has_head): (PathBuf, Change, bool)| diff_change(&root, &change, has_head)),
        "git.diff_in_commit" => handle_call(params, |(root, hash, change): (PathBuf, String, Change)| diff_in_commit(&root, &hash, &change)),
        "git.commit_files" => handle_call(params, |(root, hash): (PathBuf, String)| commit_files(&root, &hash)),
        "git.history" => handle_call(params, |(root,): (PathBuf,)| history(&root)),
        "git.branches" => handle_call(params, |(root,): (PathBuf,)| branches(&root)),
        "git.switch" => handle_call(params, |(root, branch): (PathBuf, Branch)| switch(&root, &branch)),
        "git.create_branch" => handle_call(params, |(root, name): (PathBuf, String)| create_branch(&root, &name)),
        "git.remotes" => handle_call(params, |(root,): (PathBuf,)| remotes(&root)),
        "git.remote_list" => handle_call(params, |(root,): (PathBuf,)| remote_list(&root)),
        "git.add_remote" => handle_call(params, |(root, name, url): (PathBuf, String, String)| add_remote(&root, &name, &url)),
        "git.edit_remote" => handle_call(params, |(root, name, new_name, url): (PathBuf, String, String, String)| {
            edit_remote(&root, &name, &new_name, &url)
        }),
        "git.remove_remote" => handle_call(params, |(root, name): (PathBuf, String)| remove_remote(&root, &name)),
        "git.check_remote" => handle_call(params, |(root, url): (PathBuf, String)| check_remote(&root, &url)),
        "git.fetch" => handle_call(params, |(root,): (PathBuf,)| fetch(&root)),
        "git.pull" => handle_call(params, |(root,): (PathBuf,)| pull(&root)),
        "git.publish_to" => handle_call(params, |(root, remote): (PathBuf, String)| publish_to(&root, &remote)),
        "git.push" => handle_call(params, |(root, status): (PathBuf, Status)| push(&root, &status)),
        "git.commit" => handle_call(params, |(root, summary, description, paths): (PathBuf, String, String, Option<Vec<String>>)| commit(&root, &summary, &description, paths.as_deref())),
        "git.discard" => handle_call(params, |(root, change): (PathBuf, Change)| discard(&root, &change)),
        _ => return None,
    })
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    run(root, args, &[0])
}

/// Runs git; `ok` are the exit codes that count as success.
fn run(root: &Path, args: &[&str], ok: &[i32]) -> Result<String, String> {
    let output = crate::services::command("git")
        .args(args)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        // Reading status mustn't take the index lock from a git you run.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("Can't run git: {error}."))?;
    if output.status.code().is_some_and(|code| ok.contains(&code)) {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr
            .lines()
            .map(|line| line.trim_start_matches("fatal: ").trim_start_matches("error: "))
            .filter(|line| !line.trim().is_empty() && !line.starts_with("hint:"))
            .collect::<Vec<_>>()
            .join(" ");
        Err(if message.is_empty() { format!("git {} failed.", args[0]) } else { message })
    }
}

/// The repository `folder` is in (its top folder), if any.
fn discover_here(folder: &Path) -> Option<PathBuf> {
    git(folder, &["rev-parse", "--show-toplevel"])
        .ok()
        .map(|out| PathBuf::from(out.trim()))
        .filter(|root| root.is_dir())
}

fn init_here(folder: &Path) -> Result<(), String> {
    git(folder, &["init", "--quiet"]).map(|_| ())
}

// Status -------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Untracked,
    Conflicted,
}

impl ChangeKind {
    pub fn letter(self) -> &'static str {
        match self {
            ChangeKind::Added => "A",
            ChangeKind::Modified => "M",
            ChangeKind::Deleted => "D",
            ChangeKind::Renamed => "R",
            ChangeKind::Untracked => "U",
            ChangeKind::Conflicted => "!",
        }
    }

    fn from_code(code: char) -> Option<Self> {
        match code {
            'A' | 'C' => Some(ChangeKind::Added),
            'M' | 'T' => Some(ChangeKind::Modified),
            'D' => Some(ChangeKind::Deleted),
            'R' => Some(ChangeKind::Renamed),
            _ => None,
        }
    }
}

/// A changed file, relative to the repository's top folder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub path: String,
    /// Where a renamed file came from.
    pub from: Option<String>,
    pub kind: ChangeKind,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// `None` when HEAD is detached.
    pub branch: Option<String>,
    /// The commit HEAD is at; `None` before the first commit.
    pub head: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub changes: Vec<Change>,
}

fn status_here(root: &Path) -> Result<Status, String> {
    let out = git(root, &["status", "--porcelain=v2", "--branch", "-z", "--untracked-files=all"])?;
    Ok(parse_status(&out))
}

fn parse_status(out: &str) -> Status {
    let mut status = Status::default();
    let mut fields = out.split('\0');
    while let Some(record) = fields.next() {
        if let Some(header) = record.strip_prefix("# ") {
            let (key, value) = header.split_once(' ').unwrap_or((header, ""));
            match key {
                "branch.oid" if value != "(initial)" => status.head = Some(value.to_string()),
                "branch.head" if value != "(detached)" => status.branch = Some(value.to_string()),
                "branch.upstream" => status.upstream = Some(value.to_string()),
                "branch.ab" => {
                    for part in value.split(' ') {
                        if let Some(n) = part.strip_prefix('+') {
                            status.ahead = n.parse().unwrap_or(0);
                        } else if let Some(n) = part.strip_prefix('-') {
                            status.behind = n.parse().unwrap_or(0);
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        let mut parts = record.splitn(2, ' ');
        let (Some(tag), Some(rest)) = (parts.next(), parts.next()) else {
            continue;
        };
        let change = match tag {
            // 1 XY sub mH mI mW hH hI path
            "1" => {
                let fields: Vec<&str> = rest.splitn(8, ' ').collect();
                let (Some(xy), Some(path)) = (fields.first(), fields.get(7)) else {
                    continue;
                };
                let mut codes = xy.chars();
                let (x, y) = (codes.next().unwrap_or('.'), codes.next().unwrap_or('.'));
                let kind = ChangeKind::from_code(x).or(ChangeKind::from_code(y)).unwrap_or(ChangeKind::Modified);
                Change { path: path.to_string(), from: None, kind }
            }
            // 2 XY sub mH mI mW hH hI Xscore path \0 origPath
            "2" => {
                let parts: Vec<&str> = rest.splitn(9, ' ').collect();
                let Some(path) = parts.get(8) else {
                    continue;
                };
                // The original path is the next NUL-separated field.
                let from = fields.next().filter(|from| !from.is_empty()).map(String::from);
                Change { path: path.to_string(), from, kind: ChangeKind::Renamed }
            }
            "u" => {
                let Some(path) = rest.splitn(10, ' ').nth(9) else {
                    continue;
                };
                Change { path: path.to_string(), from: None, kind: ChangeKind::Conflicted }
            }
            "?" => Change { path: rest.to_string(), from: None, kind: ChangeKind::Untracked },
            _ => continue,
        };
        status.changes.push(change);
    }
    status.changes.sort_by(|a, b| a.path.cmp(&b.path));
    status
}

// Diffs ----------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffLine {
    /// `@@ -1,4 +1,5 @@ fn main`
    Hunk(String),
    Context { old: u32, new: u32, text: String },
    Added { new: u32, text: String },
    Removed { old: u32, text: String },
    /// "Binary files differ", "\ No newline at end of file".
    Note(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diff {
    pub lines: Vec<DiffLine>,
    pub added: u32,
    pub removed: u32,
    pub truncated: bool,
}

/// The working tree's changes to `change` since HEAD (staged or not).
fn diff_change_here(root: &Path, change: &Change, has_head: bool) -> Result<Diff, String> {
    let out = if change.kind == ChangeKind::Untracked || !has_head {
        // New to git: everything is added. (Exit code 1 means "differs".)
        run(root, &["diff", "--no-color", "--no-ext-diff", "--no-index", "--", "/dev/null", &change.path], &[0, 1])?
    } else {
        let mut args = vec!["diff", "--no-color", "--no-ext-diff", "-M", "HEAD", "--"];
        if let Some(from) = &change.from {
            args.push(from);
        }
        args.push(&change.path);
        git(root, &args)?
    };
    Ok(parse_diff(&out))
}

/// What commit `hash` changed in `path`.
fn diff_in_commit_here(root: &Path, hash: &str, change: &Change) -> Result<Diff, String> {
    let parent = format!("{hash}^");
    let has_parent = git(root, &["rev-parse", "--verify", "--quiet", &parent]).is_ok();
    let mut args: Vec<&str> = if has_parent {
        vec!["diff", "--no-color", "--no-ext-diff", "-M", &parent, hash, "--"]
    } else {
        vec!["show", "--no-color", "--no-ext-diff", "--format=", hash, "--"]
    };
    if let Some(from) = &change.from {
        args.push(from);
    }
    args.push(&change.path);
    Ok(parse_diff(&git(root, &args)?))
}

/// The files commit `hash` changed (against its first parent).
fn commit_files_here(root: &Path, hash: &str) -> Result<Vec<Change>, String> {
    let parent = format!("{hash}^");
    let has_parent = git(root, &["rev-parse", "--verify", "--quiet", &parent]).is_ok();
    let out = if has_parent {
        git(root, &["diff", "--name-status", "-z", "-M", &parent, hash])?
    } else {
        git(root, &["diff-tree", "--root", "--no-commit-id", "-r", "--name-status", "-z", hash])?
    };
    let mut fields = out.split('\0').filter(|field| !field.is_empty());
    let mut changes = Vec::new();
    while let Some(code) = fields.next() {
        let letter = code.chars().next().unwrap_or('M');
        let kind = ChangeKind::from_code(letter).unwrap_or(ChangeKind::Modified);
        let change = if matches!(letter, 'R' | 'C') {
            let (Some(from), Some(path)) = (fields.next(), fields.next()) else {
                break;
            };
            Change {
                path: path.to_string(),
                from: (letter == 'R').then(|| from.to_string()),
                kind,
            }
        } else {
            let Some(path) = fields.next() else {
                break;
            };
            Change { path: path.to_string(), from: None, kind }
        };
        changes.push(change);
    }
    Ok(changes)
}

pub fn parse_diff(out: &str) -> Diff {
    let mut diff = Diff::default();
    let (mut old, mut new) = (0u32, 0u32);
    let mut in_hunk = false;
    for line in out.lines() {
        if diff.lines.len() >= DIFF_LIMIT {
            diff.truncated = true;
            break;
        }
        if let Some(header) = line.strip_prefix("@@ ") {
            // @@ -a,b +c,d @@ context
            let mut ranges = header.split(' ');
            old = ranges.next().and_then(|r| r.trim_start_matches('-').split(',').next()?.parse().ok()).unwrap_or(1);
            new = ranges.next().and_then(|r| r.trim_start_matches('+').split(',').next()?.parse().ok()).unwrap_or(1);
            in_hunk = true;
            diff.lines.push(DiffLine::Hunk(line.to_string()));
            continue;
        }
        if !in_hunk {
            if line.starts_with("Binary files") {
                diff.lines.push(DiffLine::Note("Binary file".to_string()));
            }
            continue;
        }
        if line.starts_with("diff --git") {
            in_hunk = false;
            continue;
        }
        match line.as_bytes().first() {
            Some(b'+') => {
                diff.lines.push(DiffLine::Added { new, text: line[1..].to_string() });
                new += 1;
                diff.added += 1;
            }
            Some(b'-') => {
                diff.lines.push(DiffLine::Removed { old, text: line[1..].to_string() });
                old += 1;
                diff.removed += 1;
            }
            Some(b'\\') => diff.lines.push(DiffLine::Note(line[2..].to_string())),
            _ => {
                let text = line.strip_prefix(' ').unwrap_or(line).to_string();
                diff.lines.push(DiffLine::Context { old, new, text });
                old += 1;
                new += 1;
            }
        }
    }
    diff
}

// History ----------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commit {
    pub hash: String,
    pub parents: Vec<String>,
    pub author: String,
    /// Unix seconds.
    pub time: i64,
    /// Branches and tags here: `HEAD -> main`, `origin/main`, `tag: v1`.
    pub refs: Vec<String>,
    pub subject: String,
    pub body: String,
}

impl Commit {
    pub fn short(&self) -> &str {
        &self.hash[..self.hash.len().min(7)]
    }
}

/// The newest commits of every branch, newest first, parents after
/// children (so the graph's lines only go down).
fn history_here(root: &Path) -> Result<Vec<Commit>, String> {
    let limit = format!("-n{HISTORY_LIMIT}");
    let out = git(
        root,
        &[
            "log",
            "--topo-order",
            "--exclude=refs/stash",
            "--all",
            &limit,
            "--format=%H%x1f%P%x1f%an%x1f%at%x1f%D%x1f%s%x1f%b%x1e",
        ],
    )
    // A repository with no commits has no history yet.
    .or_else(|error| if error.contains("does not have any commits") { Ok(String::new()) } else { Err(error) })?;
    Ok(out
        .split('\x1e')
        .filter_map(|record| {
            let record = record.trim_start_matches('\n');
            let mut fields = record.split('\x1f');
            let hash = fields.next()?.to_string();
            if hash.is_empty() {
                return None;
            }
            Some(Commit {
                hash,
                parents: fields.next()?.split(' ').filter(|p| !p.is_empty()).map(String::from).collect(),
                author: fields.next()?.to_string(),
                time: fields.next()?.parse().unwrap_or(0),
                refs: fields
                    .next()?
                    .split(", ")
                    .filter(|r| !r.is_empty())
                    .map(String::from)
                    .collect(),
                subject: fields.next()?.to_string(),
                body: fields.next().unwrap_or_default().trim().to_string(),
            })
        })
        .collect())
}

/// One line of the branch graph, from `(x0, y0)` to `(x1, y1)`: x in lanes,
/// y from 0 (the row's top) to 1 (its bottom); the commit's dot is at
/// y = 0.5.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Segment {
    pub x0: usize,
    pub y0: f32,
    pub x1: usize,
    pub y1: f32,
    pub color: usize,
}

/// A commit's row in the graph.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphRow {
    /// The lane the commit's dot is in.
    pub lane: usize,
    pub color: usize,
    pub segments: Vec<Segment>,
    /// Lanes in use in this row (for its width).
    pub lanes: usize,
}

/// Lays out the branch graph for `commits` (newest first, topo order).
///
/// Each lane follows one line of history down. A commit sits in the lane
/// that was waiting for it (or a new one); its first parent continues that
/// lane, other parents (merges) take a lane of their own, and lanes from
/// several children waiting for the same commit join at its dot.
pub fn graph(commits: &[Commit]) -> Vec<GraphRow> {
    let mut lanes: Vec<Option<(String, usize)>> = Vec::new();
    let mut next_color = 0;
    let mut color = || {
        next_color += 1;
        next_color - 1
    };
    let free = |lanes: &mut Vec<Option<(String, usize)>>, avoid: usize| -> usize {
        match lanes.iter().enumerate().position(|(ix, lane)| lane.is_none() && ix != avoid) {
            Some(ix) => ix,
            None => {
                lanes.push(None);
                lanes.len() - 1
            }
        }
    };
    let mut rows = Vec::with_capacity(commits.len());
    for commit in commits {
        let waiting: Vec<usize> = lanes
            .iter()
            .enumerate()
            .filter(|(_, lane)| lane.as_ref().is_some_and(|(hash, _)| *hash == commit.hash))
            .map(|(ix, _)| ix)
            .collect();
        let (lane, own_color) = match waiting.first() {
            Some(&ix) => (ix, lanes[ix].as_ref().map_or(0, |(_, c)| *c)),
            None => (free(&mut lanes, usize::MAX), color()),
        };
        let mut segments = Vec::new();
        // Lines passing by.
        for (ix, entry) in lanes.iter().enumerate() {
            if let Some((hash, c)) = entry
                && *hash != commit.hash
            {
                segments.push(Segment { x0: ix, y0: 0., x1: ix, y1: 1., color: *c });
            }
        }
        // Lines coming down into this commit.
        for &ix in &waiting {
            let c = lanes[ix].as_ref().map_or(own_color, |(_, c)| *c);
            segments.push(Segment { x0: ix, y0: 0., x1: lane, y1: 0.5, color: c });
            lanes[ix] = None;
        }
        let before = lanes.len();
        for (nth, parent) in commit.parents.iter().enumerate() {
            if nth == 0 {
                lanes[lane] = Some((parent.clone(), own_color));
                segments.push(Segment { x0: lane, y0: 0.5, x1: lane, y1: 1., color: own_color });
            } else if let Some(ix) = lanes.iter().position(|l| l.as_ref().is_some_and(|(hash, _)| hash == parent)) {
                // Merging in a line that's already on its way.
                let c = lanes[ix].as_ref().map_or(own_color, |(_, c)| *c);
                segments.push(Segment { x0: lane, y0: 0.5, x1: ix, y1: 1., color: c });
            } else {
                let ix = free(&mut lanes, lane);
                let c = color();
                lanes[ix] = Some((parent.clone(), c));
                segments.push(Segment { x0: lane, y0: 0.5, x1: ix, y1: 1., color: c });
            }
        }
        let width = before.max(lanes.len()).max(lane + 1);
        while lanes.last().is_some_and(Option::is_none) {
            lanes.pop();
        }
        rows.push(GraphRow { lane, color: own_color, segments, lanes: width });
    }
    rows
}

// Branches and syncing ---------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Branch {
    pub name: String,
    pub current: bool,
    /// A remote branch (`origin/feature`) with no local one yet.
    pub remote: bool,
}

fn branches_here(root: &Path) -> Result<Vec<Branch>, String> {
    let out = git(root, &["for-each-ref", "--format=%(HEAD)%(refname)", "refs/heads", "refs/remotes"])?;
    let mut local = Vec::new();
    let mut remote = Vec::new();
    for line in out.lines() {
        let current = line.starts_with('*');
        let name = line.trim_start_matches(['*', ' ']);
        if let Some(name) = name.strip_prefix("refs/heads/") {
            local.push(Branch { name: name.to_string(), current, remote: false });
        } else if let Some(name) = name.strip_prefix("refs/remotes/") {
            if !name.ends_with("/HEAD") {
                remote.push(name.to_string());
            }
        }
    }
    // A remote branch only shows if there's no local branch of that name.
    let names: Vec<String> = local.iter().map(|b| b.name.clone()).collect();
    for name in remote {
        let short = name.split_once('/').map_or(name.as_str(), |(_, rest)| rest);
        if !names.iter().any(|n| n == short) {
            local.push(Branch { name, current: false, remote: true });
        }
    }
    Ok(local)
}

fn switch_here(root: &Path, branch: &Branch) -> Result<(), String> {
    if branch.remote {
        // Tracks the remote branch under its own name.
        git(root, &["switch", "--track", &branch.name]).map(|_| ())
    } else {
        git(root, &["switch", &branch.name]).map(|_| ())
    }
}

fn create_branch_here(root: &Path, name: &str) -> Result<(), String> {
    git(root, &["switch", "-c", name.trim()]).map(|_| ())
}

fn remotes_here(root: &Path) -> Vec<String> {
    git(root, &["remote"]).map(|out| out.lines().map(String::from).collect()).unwrap_or_default()
}

/// A remote: where git fetches from and pushes to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remote {
    pub name: String,
    pub url: String,
    /// Where pushes go, when that's somewhere else.
    pub push_url: Option<String>,
}

fn remote_list_here(root: &Path) -> Result<Vec<Remote>, String> {
    Ok(parse_remotes(&git(root, &["remote", "--verbose"])?))
}

/// `git remote --verbose`: `name\turl (fetch)`, then `(push)`.
fn parse_remotes(out: &str) -> Vec<Remote> {
    let mut remotes: Vec<Remote> = Vec::new();
    for line in out.lines() {
        let Some((name, rest)) = line.split_once('\t') else {
            continue;
        };
        let (url, kind) = rest.rsplit_once(' ').unwrap_or((rest, "(fetch)"));
        match remotes.iter_mut().find(|remote| remote.name == name) {
            Some(remote) if kind == "(push)" && remote.url != url => remote.push_url = Some(url.to_string()),
            Some(_) => {}
            None => remotes.push(Remote { name: name.to_string(), url: url.to_string(), push_url: None }),
        }
    }
    remotes
}

/// A remote's name and URL, checked so neither reads as an option.
fn check_names(name: &str, url: &str) -> Result<(String, String), String> {
    let (name, url) = (name.trim(), url.trim());
    if name.is_empty() {
        return Err("Give the remote a name (usually “origin”).".into());
    }
    if name.starts_with('-') || name.chars().any(char::is_whitespace) {
        return Err("A remote's name can't have spaces or start with “-”.".into());
    }
    if url.is_empty() {
        return Err("Paste the repository's URL.".into());
    }
    if url.starts_with('-') {
        return Err("That isn't a repository URL.".into());
    }
    Ok((name.to_string(), url.to_string()))
}

fn add_remote_here(root: &Path, name: &str, url: &str) -> Result<(), String> {
    let (name, url) = check_names(name, url)?;
    git(root, &["remote", "add", &name, &url]).map(|_| ())
}

fn edit_remote_here(root: &Path, name: &str, new_name: &str, url: &str) -> Result<(), String> {
    let (new_name, url) = check_names(new_name, url)?;
    if new_name != name {
        git(root, &["remote", "rename", name, &new_name])?;
    }
    git(root, &["remote", "set-url", &new_name, &url]).map(|_| ())
}

fn remove_remote_here(root: &Path, name: &str) -> Result<(), String> {
    git(root, &["remote", "remove", name]).map(|_| ())
}

fn check_remote_here(root: &Path, url: &str) -> Result<usize, String> {
    let (_, url) = check_names("check", url)?;
    let out = git(root, &["ls-remote", "--heads", &url])?;
    Ok(out.lines().filter(|line| !line.trim().is_empty()).count())
}

fn fetch_here(root: &Path) -> Result<(), String> {
    git(root, &["fetch", "--all", "--prune", "--quiet"]).map(|_| ())
}

fn pull_here(root: &Path) -> Result<(), String> {
    git(root, &["pull", "--quiet"]).map(|_| ())
}

/// Pushes the branch; one with no upstream yet is published to `origin` (or
/// the first remote).
fn push_here(root: &Path, status: &Status) -> Result<(), String> {
    if status.upstream.is_some() {
        return git(root, &["push", "--quiet"]).map(|_| ());
    }
    let remotes = remotes_here(root);
    let remote = remotes
        .iter()
        .find(|name| *name == "origin")
        .or(remotes.first())
        .ok_or("There's no remote to publish to.")?;
    git(root, &["push", "--quiet", "--set-upstream", remote, "HEAD"]).map(|_| ())
}

fn publish_to_here(root: &Path, remote: &str) -> Result<(), String> {
    git(root, &["push", "--quiet", "--set-upstream", remote, "HEAD"]).map(|_| ())
}

/// Commits the changes to `paths` (all changes when `paths` is `None`).
fn commit_here(root: &Path, summary: &str, description: &str, paths: Option<&[String]>) -> Result<(), String> {
    let mut message = summary.trim().to_string();
    if !description.trim().is_empty() {
        message.push_str("\n\n");
        message.push_str(description.trim());
    }
    match paths {
        None => {
            git(root, &["add", "--all"])?;
            git(root, &["commit", "--quiet", "--message", &message])?;
        }
        Some(paths) => {
            let mut add = vec!["add", "--all", "--"];
            add.extend(paths.iter().map(String::as_str));
            git(root, &add)?;
            let mut commit = vec!["commit", "--quiet", "--message", &message, "--"];
            commit.extend(paths.iter().map(String::as_str));
            git(root, &commit)?;
        }
    }
    Ok(())
}

/// Throws away the working tree's changes to `change`.
fn discard_here(root: &Path, change: &Change) -> Result<(), String> {
    match change.kind {
        ChangeKind::Untracked => git(root, &["clean", "--force", "--quiet", "--", &change.path]).map(|_| ()),
        ChangeKind::Added => git(root, &["rm", "--force", "--quiet", "--", &change.path]).map(|_| ()),
        _ => {
            let mut args = vec!["restore", "--source=HEAD", "--staged", "--worktree", "--"];
            if let Some(from) = &change.from {
                args.push(from);
            }
            args.push(&change.path);
            git(root, &args)?;
            if change.kind == ChangeKind::Renamed {
                // The renamed copy is left behind, untracked.
                git(root, &["clean", "--force", "--quiet", "--", &change.path]).ok();
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(hash: &str, parents: &[&str]) -> Commit {
        Commit {
            hash: hash.into(),
            parents: parents.iter().map(|p| p.to_string()).collect(),
            author: String::new(),
            time: 0,
            refs: Vec::new(),
            subject: hash.into(),
            body: String::new(),
        }
    }

    #[test]
    fn status_reads_branch_and_changes() {
        let out = "# branch.oid 1234abcd\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +2 -1\0\
                   1 .M N... 100644 100644 100644 aaa bbb src/main.rs\0\
                   2 R. N... 100644 100644 100644 aaa bbb R100 new.rs\0old.rs\0\
                   ? notes.txt\0";
        let status = parse_status(out);
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(status.head.as_deref(), Some("1234abcd"));
        assert_eq!((status.ahead, status.behind), (2, 1));
        assert_eq!(status.changes.len(), 3);
        let renamed = status.changes.iter().find(|c| c.path == "new.rs").unwrap();
        assert_eq!((renamed.kind, renamed.from.as_deref()), (ChangeKind::Renamed, Some("old.rs")));
        let untracked = status.changes.iter().find(|c| c.path == "notes.txt").unwrap();
        assert_eq!(untracked.kind, ChangeKind::Untracked);
        assert_eq!(status.changes.iter().find(|c| c.path == "src/main.rs").unwrap().kind, ChangeKind::Modified);
    }

    #[test]
    fn diffs_number_their_lines() {
        let out = "diff --git a/a b/a\nindex 1..2 100644\n--- a/a\n+++ b/a\n@@ -1,3 +1,3 @@ fn x\n one\n-two\n+TWO\n three\n";
        let diff = parse_diff(out);
        assert_eq!((diff.added, diff.removed), (1, 1));
        assert_eq!(diff.lines[1], DiffLine::Context { old: 1, new: 1, text: "one".into() });
        assert_eq!(diff.lines[2], DiffLine::Removed { old: 2, text: "two".into() });
        assert_eq!(diff.lines[3], DiffLine::Added { new: 2, text: "TWO".into() });
        assert_eq!(diff.lines[4], DiffLine::Context { old: 3, new: 3, text: "three".into() });
    }

    #[test]
    fn the_graph_branches_and_merges() {
        // m merges f into the main line:  m ─ a ─ root
        //                                  └ f ┘
        let commits = [commit("m", &["a", "f"]), commit("f", &["root"]), commit("a", &["root"]), commit("root", &[])];
        let rows = graph(&commits);
        assert_eq!(rows[0].lane, 0);
        assert_eq!(rows[1].lane, 1, "the merged branch gets a lane of its own");
        assert_eq!(rows[2].lane, 0);
        assert_eq!(rows[3].lane, 0, "both lines join at the root");
        assert!(rows[3].segments.iter().any(|s| s.x0 == 1 && s.x1 == 0 && s.y1 == 0.5));
        assert!(rows.iter().all(|row| row.lanes <= 2));
    }

    #[test]
    fn a_real_repository_works_end_to_end() {
        let root = std::env::temp_dir().join(format!("xh-git-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(&root).unwrap();
        init(&root).unwrap();
        git(&root, &["config", "user.name", "Test"]).unwrap();
        git(&root, &["config", "user.email", "test@example.com"]).unwrap();
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        std::fs::write(root.join("b.txt"), "two\n").unwrap();

        let status = status(&root).unwrap();
        assert_eq!(status.changes.len(), 2);
        assert!(status.head.is_none(), "no commits yet");
        let diff = diff_change(&root, &status.changes[0], false).unwrap();
        assert_eq!(diff.added, 1);

        // Only a.txt goes in the first commit.
        super::commit(&root, "First", "", Some(&["a.txt".to_string()])).unwrap();
        let status = super::status(&root).unwrap();
        assert_eq!(status.changes.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(), ["b.txt"]);
        create_branch(&root, "feature").unwrap();
        super::commit(&root, "Second", "with a body", None).unwrap();

        let history = history(&root).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!((history[0].subject.as_str(), history[0].body.as_str()), ("Second", "with a body"));
        assert!(history[0].refs.iter().any(|r| r.contains("feature")));
        assert_eq!(commit_files(&root, &history[0].hash).unwrap()[0].path, "b.txt");
        assert_eq!(commit_files(&root, &history[1].hash).unwrap()[0].path, "a.txt", "the root commit too");
        let diff = diff_in_commit(&root, &history[0].hash, &commit_files(&root, &history[0].hash).unwrap()[0]).unwrap();
        assert_eq!(diff.added, 1);
        let names: Vec<String> = branches(&root).unwrap().into_iter().map(|b| b.name).collect();
        assert!(names.contains(&"feature".to_string()));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn remotes_are_read_with_their_push_urls() {
        let out = "origin\thttps://github.com/a/b.git (fetch)\norigin\tgit@github.com:a/b.git (push)\n\
                   backup\t/srv/b.git (fetch)\nbackup\t/srv/b.git (push)\n";
        let remotes = parse_remotes(out);
        assert_eq!(remotes.len(), 2);
        assert_eq!(remotes[0].url, "https://github.com/a/b.git");
        assert_eq!(remotes[0].push_url.as_deref(), Some("git@github.com:a/b.git"));
        assert_eq!((remotes[1].name.as_str(), remotes[1].push_url.as_ref()), ("backup", None));
    }

    /// Adding a remote, checking it, publishing to it, renaming and
    /// removing it, with a bare repository on disk as the "server".
    #[test]
    fn remotes_can_be_added_published_to_and_removed() {
        let base = std::env::temp_dir().join(format!("xh-git-remote-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let (root, bare) = (base.join("work"), base.join("shared.git"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "--bare", "--quiet"]).unwrap();
        init(&root).unwrap();
        git(&root, &["config", "user.name", "Test"]).unwrap();
        git(&root, &["config", "user.email", "test@example.com"]).unwrap();
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        super::commit(&root, "First", "", None).unwrap();

        let url = bare.display().to_string();
        assert!(add_remote(&root, "-x", &url).is_err(), "names can't look like options");
        assert!(add_remote(&root, "origin", "--upload-pack=x").is_err());
        assert_eq!(check_remote(&root, &url).unwrap(), 0, "empty so far");
        add_remote(&root, "origin", &url).unwrap();
        assert_eq!(remote_list(&root).unwrap()[0].url, url);

        push(&root, &status(&root).unwrap()).unwrap();
        assert!(status(&root).unwrap().upstream.is_some(), "published");
        assert_eq!(check_remote(&root, &url).unwrap(), 1);
        assert!(check_remote(&root, &base.join("nothing").display().to_string()).is_err());

        edit_remote(&root, "origin", "shared", &url).unwrap();
        assert_eq!(remote_list(&root).unwrap()[0].name, "shared");
        remove_remote(&root, "shared").unwrap();
        assert!(remote_list(&root).unwrap().is_empty());
        std::fs::remove_dir_all(&base).ok();
    }
}

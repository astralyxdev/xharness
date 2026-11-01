//! Files, for the app's windows: listing folders (the Explorer), searching
//! them, moving and copying, reading and writing text (the Editor, project
//! layouts), and noticing changes. All of it runs in the server (see
//! `rpc.rs`), on the machine the files are on.

use std::{
    fs, io,
    path::{Path, PathBuf},
    time::SystemTime,
};

use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::services::rpc::{handle_call, route};

/// Search results stop here.
pub const SEARCH_LIMIT: usize = 2000;
/// Never listed: git's internals and Finder's litter.
const ALWAYS_HIDDEN: [&str; 2] = [".git", ".DS_Store"];
/// Bigger files aren't opened for editing.
const MAX_TEXT: u64 = 16 * 1024 * 1024;
/// "Can't rename across devices": EXDEV, or Windows' ERROR_NOT_SAME_DEVICE.
#[cfg(unix)]
const CROSS_DEVICE: i32 = libc::EXDEV;
#[cfg(windows)]
const CROSS_DEVICE: i32 = 17;

// Routed -----------------------------------------------------------------------------

/// A folder's entries (not sorted).
pub fn list_dir(dir: &Path, options: ListOptions) -> Vec<Entry> {
    route("files.list_dir", (dir, options), || list_dir_here(dir, options))
}

/// Everything under `root` matching `query` (see [`Pattern`]), up to
/// [`SEARCH_LIMIT`]; and whether there was more.
pub fn search(root: &Path, query: &str, options: ListOptions) -> (Vec<Entry>, bool) {
    route("files.search", (root, query, options), || match Pattern::new(query) {
        Some(pattern) => search_here(root, &pattern, options),
        None => (Vec::new(), false),
    })
}

/// Copies (or moves) `sources` into the folder `into`; a name that's taken
/// gets " copy" (" copy 2", …) added, as in Finder. Returns where they went.
pub fn transfer(sources: &[PathBuf], into: &Path, copy: bool) -> Result<Vec<PathBuf>, String> {
    route("files.transfer", (sources, into, copy), || transfer_here(sources, into, copy))
}

/// What a file on disk looked like when it was last read or written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    pub modified: Option<SystemTime>,
    pub len: u64,
}

impl Stamp {
    fn of(meta: &fs::Metadata) -> Self {
        Self { modified: meta.modified().ok(), len: meta.len() }
    }
}

/// A text file, read for editing.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Loaded {
    /// With `\n` line endings.
    pub text: String,
    /// It had `\r\n` endings.
    pub crlf: bool,
    pub stamp: Stamp,
}

/// The file's stamp now (`None` if it's gone).
pub fn stamp(path: &Path) -> Option<Stamp> {
    route("files.stamp", (path,), || fs::metadata(path).ok().map(|meta| Stamp::of(&meta)))
}

/// Reads a text file for editing (not folders, binaries, or huge files).
pub fn read_text(path: &Path) -> Result<Loaded, String> {
    route("files.read_text", (path,), || read_text_here(path))
}

/// Writes through a temp file and a rename (so a crash can't leave half a
/// file), onto the file a symlink points at, keeping its permissions.
pub fn write_text(path: &Path, contents: &str) -> Result<Stamp, String> {
    route("files.write_text", (path, contents), || write_text_here(path, contents).map_err(|e| e.to_string()))
}

/// A small file's contents, `None` if it isn't there.
pub fn read(path: &Path) -> Result<Option<String>, String> {
    route("files.read", (path,), || match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    })
}

/// Writes `contents` to `path` (creating folders) through a temp file and a
/// rename, so a crash can't leave it truncated.
pub fn write(path: &Path, contents: &str) -> Result<(), String> {
    route("files.write", (path, contents), || write_here(path, contents).map_err(|e| e.to_string()))
}

/// The home folder where the files are (for `~/…` paths).
pub fn home() -> Option<PathBuf> {
    route("files.home", (), std::env::home_dir)
}

/// The server runs these.
pub(crate) fn dispatch(method: &str, params: Value) -> Option<Result<Value, String>> {
    Some(match method {
        "files.list_dir" => handle_call(params, |(dir, options): (PathBuf, ListOptions)| list_dir(&dir, options)),
        "files.search" => handle_call(params, |(root, query, options): (PathBuf, String, ListOptions)| {
            search(&root, &query, options)
        }),
        "files.transfer" => handle_call(params, |(sources, into, copy): (Vec<PathBuf>, PathBuf, bool)| {
            transfer(&sources, &into, copy)
        }),
        "files.stamp" => handle_call(params, |(path,): (PathBuf,)| stamp(&path)),
        "files.read_text" => handle_call(params, |(path,): (PathBuf,)| read_text(&path)),
        "files.write_text" => handle_call(params, |(path, contents): (PathBuf, String)| write_text(&path, &contents)),
        "files.read" => handle_call(params, |(path,): (PathBuf,)| read(&path)),
        "files.home" => handle_call(params, |(): ()| home()),
        "files.write" => handle_call(params, |(path, contents): (PathBuf, String)| write(&path, &contents)),
        _ => return None,
    })
}

// Here (in the server) ----------------------------------------------------------------

fn read_text_here(path: &Path) -> Result<Loaded, String> {
    let meta = fs::metadata(path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => "This file isn't there any more.".to_string(),
        _ => format!("Can't open this file: {error}."),
    })?;
    if meta.is_dir() {
        return Err("This is a folder.".to_string());
    }
    if meta.len() > MAX_TEXT {
        return Err(format!("This file is too big to edit here ({} MB).", meta.len() / 1_000_000));
    }
    let bytes = fs::read(path).map_err(|error| format!("Can't open this file: {error}."))?;
    if bytes[..bytes.len().min(8000)].contains(&0) {
        return Err("This is a binary file.".to_string());
    }
    let text = String::from_utf8(bytes).map_err(|_| "This file isn't UTF-8 text.".to_string())?;
    let crlf = text.contains("\r\n");
    Ok(Loaded {
        text: if crlf { text.replace("\r\n", "\n") } else { text },
        crlf,
        stamp: Stamp::of(&meta),
    })
}

fn write_text_here(path: &Path, contents: &str) -> io::Result<Stamp> {
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target.parent().unwrap_or(Path::new("."));
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let temp = dir.join(format!(".{name}.xharness-{}", std::process::id()));
    fs::write(&temp, contents)?;
    if let Ok(meta) = fs::metadata(&target) {
        fs::set_permissions(&temp, meta.permissions()).ok();
    }
    if let Err(error) = fs::rename(&temp, &target) {
        fs::remove_file(&temp).ok();
        return Err(error);
    }
    Ok(Stamp::of(&fs::metadata(&target)?))
}

fn write_here(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    fs::write(&temp, contents)?;
    fs::rename(&temp, path)
}

/// Copies (or moves) `sources` into the folder `into`; a name that's taken
/// gets " copy" (" copy 2", …) added, as in Finder. Returns where they went.
fn transfer_here(sources: &[PathBuf], into: &Path, copy: bool) -> Result<Vec<PathBuf>, String> {
    let mut placed = Vec::new();
    for source in sources {
        let Some(name) = source.file_name() else {
            continue;
        };
        if !copy && source.parent() == Some(into) {
            continue;
        }
        if into.starts_with(source) {
            return Err(format!("Can't put {} inside itself.", name.to_string_lossy()));
        }
        let target = free_name(into, &name.to_string_lossy());
        let result = if copy {
            copy_all(source, &target)
        } else {
            fs::rename(source, &target).or_else(|error| {
                // Another disk: copy, then remove the original.
                if error.raw_os_error() == Some(CROSS_DEVICE) {
                    copy_all(source, &target)?;
                    if source.is_dir() { fs::remove_dir_all(source) } else { fs::remove_file(source) }
                } else {
                    Err(error)
                }
            })
        };
        result.map_err(|error| format!("Couldn't {} {}: {error}.", if copy { "copy" } else { "move" }, name.to_string_lossy()))?;
        placed.push(target);
    }
    Ok(placed)
}

fn free_name(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if fs::symlink_metadata(&candidate).is_err() {
        return candidate;
    }
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, format!(".{extension}")),
        _ => (name, String::new()),
    };
    (1..)
        .map(|n| {
            let suffix = if n == 1 { " copy".to_string() } else { format!(" copy {n}") };
            dir.join(format!("{stem}{suffix}{extension}"))
        })
        .find(|path| fs::symlink_metadata(path).is_err())
        .expect("some name is free")
}

fn copy_all(from: &Path, to: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(from)?;
    if meta.file_type().is_symlink() {
        let target = fs::read_link(from)?;
        #[cfg(unix)]
        return std::os::unix::fs::symlink(target, to);
        #[cfg(windows)]
        return if from.is_dir() {
            std::os::windows::fs::symlink_dir(target, to)
        } else {
            std::os::windows::fs::symlink_file(target, to)
        };
    } else if meta.is_dir() {
        fs::create_dir(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_all(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        fs::copy(from, to).map(|_| ())
    }
}



#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ListOptions {
    pub show_hidden: bool,
    pub show_ignored: bool,
}

fn walker(dir: &Path, options: ListOptions) -> WalkBuilder {
    let respect_ignores = !options.show_ignored;
    let mut builder = WalkBuilder::new(dir);
    builder
        .hidden(!options.show_hidden)
        .ignore(respect_ignores)
        .git_ignore(respect_ignores)
        .git_global(respect_ignores)
        .git_exclude(respect_ignores)
        // Rules from parent folders apply too, and outside a git
        // repository as well (a project folder isn't always one).
        .parents(true)
        .require_git(false)
        .follow_links(false)
        .filter_entry(|entry| !ALWAYS_HIDDEN.contains(&entry.file_name().to_string_lossy().as_ref()));
    builder
}

fn entry_from(dir_entry: &ignore::DirEntry) -> Entry {
    let path = dir_entry.path().to_path_buf();
    // Follow symlinks for what they point at: a link to a folder opens like
    // one.
    let metadata = std::fs::metadata(&path).ok();
    let is_dir = metadata.as_ref().is_some_and(|m| m.is_dir());
    Entry {
        name: dir_entry.file_name().to_string_lossy().into_owned(),
        is_dir,
        size: metadata.as_ref().filter(|m| !m.is_dir()).map_or(0, |m| m.len()),
        modified: metadata.and_then(|m| m.modified().ok()),
        path,
    }
}

/// A folder's contents (unsorted).
fn list_dir_here(dir: &Path, options: ListOptions) -> Vec<Entry> {
    walker(dir, options)
        .max_depth(Some(1))
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.depth() == 1)
        .map(|entry| entry_from(&entry))
        .collect()
}

/// Everything under `root` that matches, up to [`SEARCH_LIMIT`]. Also says
/// whether it stopped at the limit.
fn search_here(root: &Path, pattern: &Pattern, options: ListOptions) -> (Vec<Entry>, bool) {
    let mut results = Vec::new();
    for entry in walker(root, options).build().filter_map(Result::ok) {
        if entry.depth() == 0 {
            continue;
        }
        let relative = entry.path().strip_prefix(root).unwrap_or(entry.path());
        if pattern.matches(&entry.file_name().to_string_lossy(), &relative.to_string_lossy()) {
            results.push(entry_from(&entry));
            if results.len() >= SEARCH_LIMIT {
                return (results, true);
            }
        }
    }
    (results, false)
}

/// What the filter field matches.
#[derive(Clone, Debug)]
pub(crate) struct Pattern {
    /// Lowercased.
    text: String,
    wildcard: bool,
    /// Matches the path from the project folder rather than the name.
    path: bool,
}

impl Pattern {
    pub(crate) fn new(query: &str) -> Option<Self> {
        let text = query.trim().to_lowercase();
        (!text.is_empty()).then(|| Pattern {
            wildcard: text.contains('*') || text.contains('?'),
            path: text.contains('/'),
            text,
        })
    }

    pub(crate) fn matches(&self, name: &str, relative: &str) -> bool {
        let subject = if self.path { relative } else { name }.to_lowercase();
        if self.wildcard {
            let pattern: Vec<char> = self.text.chars().collect();
            let subject: Vec<char> = subject.chars().collect();
            wildcard_match(&pattern, &subject)
        } else {
            subject.contains(&self.text)
        }
    }
}

/// `*` matches any run of characters, `?` any one.
fn wildcard_match(pattern: &[char], subject: &[char]) -> bool {
    let (mut p, mut s) = (0, 0);
    // Where the last `*` was, and how much of the subject it has taken.
    let mut star: Option<(usize, usize)> = None;
    while s < subject.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == subject[s]) {
            p += 1;
            s += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, s));
            p += 1;
        } else if let Some((star_p, star_s)) = star {
            // Let the `*` take one more character and try again.
            p = star_p + 1;
            s = star_s + 1;
            star = Some((star_p, star_s + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards_and_substrings_match() {
        let matches = |query: &str, name: &str, relative: &str| Pattern::new(query).unwrap().matches(name, relative);
        assert!(matches("main", "main.rs", "src/main.rs"));
        assert!(matches("MAIN", "main.rs", "src/main.rs"), "ignores case");
        assert!(matches("*.rs", "main.rs", "src/main.rs"));
        assert!(!matches("*.rs", "main.rsx", "src/main.rsx"));
        assert!(matches("m??n.*", "main.rs", "src/main.rs"));
        assert!(matches("src/*.rs", "main.rs", "src/main.rs"), "a / matches the path");
        assert!(!matches("src/*.rs", "main.rs", "tests/main.rs"));
        assert!(Pattern::new("   ").is_none());
    }


    #[test]
    fn files_move_and_copy_without_clobbering() {
        let root = std::env::temp_dir().join(format!("xh-transfer-{}", std::process::id()));
        fs::remove_dir_all(&root).ok();
        let (from, to) = (root.join("from"), root.join("to"));
        fs::create_dir_all(from.join("dir/inner")).unwrap();
        fs::create_dir_all(&to).unwrap();
        fs::write(from.join("a.txt"), "a").unwrap();
        fs::write(from.join("dir/inner/b.txt"), "b").unwrap();
        fs::write(to.join("a.txt"), "taken").unwrap();

        let placed = transfer_here(&[from.join("a.txt"), from.join("dir")], &to, true).unwrap();
        assert_eq!(placed, [to.join("a copy.txt"), to.join("dir")]);
        assert_eq!(fs::read_to_string(to.join("dir/inner/b.txt")).unwrap(), "b", "folders copy whole");
        assert!(from.join("a.txt").exists(), "copying leaves the original");

        let placed = transfer_here(&[from.join("a.txt")], &to, false).unwrap();
        assert_eq!(placed, [to.join("a copy 2.txt")]);
        assert!(!from.join("a.txt").exists(), "moving doesn't");
        assert!(transfer_here(&[to.join("dir")], &to.join("dir/inner"), false).is_err(), "not into itself");
        fs::remove_dir_all(&root).ok();
    }


    /// Listing applies .gitignore, hides dotfiles on request, and never
    /// shows .git; search walks the whole tree.
    #[test]
    fn listing_and_search_respect_the_view_options() {
        let root = std::env::temp_dir().join(format!("xh-explorer-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        for dir in ["src/nested", ".git", "target"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        for file in [".gitignore", ".env", "README.md", "src/main.rs", "src/nested/deep.rs", "target/out.rs"] {
            std::fs::write(root.join(file), "x").unwrap();
        }
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        let names = |options: ListOptions| {
            let mut names: Vec<String> = list_dir_here(&root, options).into_iter().map(|e| e.name).collect();
            names.sort();
            names
        };

        let everything = ListOptions { show_hidden: true, show_ignored: true };
        assert_eq!(names(everything), [".env", ".gitignore", "README.md", "src", "target"]);
        let tidy = ListOptions { show_hidden: false, show_ignored: false };
        assert_eq!(names(tidy), ["README.md", "src"]);

        let (found, truncated) = search_here(&root, &Pattern::new("*.rs").unwrap(), tidy);
        let mut found: Vec<String> = found.into_iter().map(|e| e.name).collect();
        found.sort();
        assert_eq!((found, truncated), (vec!["deep.rs".to_string(), "main.rs".to_string()], false));
        let (with_ignored, _) = search_here(&root, &Pattern::new("*.rs").unwrap(), everything);
        assert_eq!(with_ignored.len(), 3, "target/out.rs too");
    }

    #[test]
    #[cfg(unix)]
    fn saving_keeps_permissions_and_follows_links() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("xh-editor-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("run.sh");
        fs::write(&file, "echo hi\n").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        let link = dir.join("link.sh");
        std::os::unix::fs::symlink(&file, &link).unwrap();

        write_text(&link, "echo bye\n").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "echo bye\n");
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "still a link");
        assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o755);

        let loaded = read_text(&file).unwrap();
        assert_eq!((loaded.text.as_str(), loaded.crlf), ("echo bye\n", false));
        fs::write(dir.join("dos.txt"), "a\r\nb\r\n").unwrap();
        let dos = read_text(&dir.join("dos.txt")).unwrap();
        assert_eq!((dos.text.as_str(), dos.crlf), ("a\nb\n", true));
        fs::write(dir.join("blob"), [0u8, 1, 2]).unwrap();
        assert!(read_text(&dir.join("blob")).is_err());
        fs::remove_dir_all(&dir).ok();
    }
}

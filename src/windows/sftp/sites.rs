//! The SFTP servers you've added (FileZilla's Site Manager), for every
//! project's SFTP windows. Saved in `sftp-sites.json` in the data
//! directory; it holds passwords, so only you can read it.

use gpui::{App, BorrowAppContext, Global};
use serde::{Deserialize, Serialize};

use crate::services::sftp::Site;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedSite {
    pub id: String,
    pub name: String,
    #[serde(flatten)]
    pub site: Site,
    /// The folder to open on the server; empty for the one it starts in.
    #[serde(default)]
    pub remote_dir: String,
}

impl SavedSite {
    /// What a list shows: its name, or where it goes.
    pub fn label(&self) -> String {
        if self.name.trim().is_empty() { self.site.address() } else { self.name.clone() }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct SitesFile {
    #[serde(default)]
    sites: Vec<SavedSite>,
}

/// The servers (a global; windows observe it).
#[derive(Default)]
pub struct Sites {
    pub list: Vec<SavedSite>,
}

impl Global for Sites {}

impl Sites {
    /// Reads the file the first time it's needed.
    pub fn ensure(cx: &mut App) {
        if cx.has_global::<Sites>() {
            return;
        }
        let list = path()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|json| serde_json::from_str::<SitesFile>(&json).ok())
            .map(|file| file.sites)
            .unwrap_or_default();
        cx.set_global(Sites { list });
    }

    pub fn all(cx: &App) -> Vec<SavedSite> {
        cx.try_global::<Sites>().map(|sites| sites.list.clone()).unwrap_or_default()
    }

    pub fn get(id: &str, cx: &App) -> Option<SavedSite> {
        cx.try_global::<Sites>()?.list.iter().find(|site| site.id == id).cloned()
    }

    /// Adds `site`, or replaces the one with its id, and saves.
    pub fn put(site: SavedSite, cx: &mut App) {
        Self::ensure(cx);
        cx.update_global::<Sites, _>(|sites, _| {
            match sites.list.iter_mut().find(|s| s.id == site.id) {
                Some(existing) => *existing = site,
                None => sites.list.push(site),
            }
            save(&sites.list);
        });
    }

    pub fn remove(id: &str, cx: &mut App) {
        Self::ensure(cx);
        cx.update_global::<Sites, _>(|sites, _| {
            sites.list.retain(|site| site.id != id);
            save(&sites.list);
        });
    }
}

/// A new site's id.
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn path() -> Option<std::path::PathBuf> {
    crate::services::paths::data_dir().map(|dir| dir.join("sftp-sites.json"))
}

fn save(list: &[SavedSite]) {
    let Some(path) = path() else {
        return;
    };
    let file = SitesFile { sites: list.to_vec() };
    let Ok(mut json) = serde_json::to_vec_pretty(&file) else {
        return;
    };
    json.push(b'\n');
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    write_private(&path, &json).ok();
}

/// Writes `bytes` to `path` (via a temp file and a rename), readable and
/// writable only by you.
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let temp = path.with_extension("json.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temp, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // A file from before (or made by hand) may be readable by others.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).ok();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::sftp::Auth;

    #[test]
    fn sites_are_saved_flat_and_read_back() {
        let site = SavedSite {
            id: "a".into(),
            name: "Production".into(),
            site: Site {
                host: "example.com".into(),
                port: 22,
                user: "deploy".into(),
                auth: Auth::Password { password: "secret".into() },
            },
            remote_dir: "/var/www".into(),
        };
        let json = serde_json::to_string(&SitesFile { sites: vec![site.clone()] }).unwrap();
        assert!(json.contains(r#""host":"example.com""#), "{json}");
        let back: SitesFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sites, [site]);
    }

    #[cfg(unix)]
    #[test]
    fn only_you_can_read_the_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("xh-sites-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sftp-sites.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&path, b"{\"sites\":[]}").unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::remove_dir_all(&dir).ok();
    }
}

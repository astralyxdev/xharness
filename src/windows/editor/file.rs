//! Reading and writing the file, and noticing when it changes on disk: the
//! files service does it, in the server (see `services/files.rs`).

pub(super) use crate::services::files::{Loaded, Stamp, read_text as read_file, write_text as write_file};

/// The file's stamp on disk now.
pub(super) fn stamp_of(path: &std::path::Path) -> Option<Stamp> {
    crate::services::files::stamp(path)
}

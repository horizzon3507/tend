//! Reversible deletion: removed directories land in a trash store first, so a
//! mistake is one `tend trash restore` away instead of gone forever.
//!
//! Layout under the data dir:
//!
//! ```text
//! trash/
//!   items/<id>-<name>   the moved directory itself
//!   manifest.jsonl      one entry per moved item, append-only
//! ```
//!
//! The manifest is best effort, like the cleanup history: the item on disk is
//! the source of truth, and listing filters to entries whose item still exists.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config;
use crate::util::{ensure_parent, now_ms};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrashEntry {
    pub id: String,
    /// directory name under `items/`, so the entry finds its payload
    pub stored: String,
    pub original_path: String,
    pub trashed_at: i64,
    pub bytes: u64,
    /// the command that collected it (`clean`, `projects`)
    pub source: String,
}

pub fn trash_dir() -> PathBuf {
    config::data_dir().join("trash")
}

fn items_dir_in(base: &Path) -> PathBuf {
    base.join("items")
}

fn manifest_path_in(base: &Path) -> PathBuf {
    base.join("manifest.jsonl")
}

fn short_hash(path: &Path) -> String {
    let digest = Sha256::digest(path.display().to_string().as_bytes());
    digest.iter().take(2).map(|b| format!("{b:02x}")).collect()
}

/// Moves a directory without following symlinks: rename when both sides share a
/// filesystem (atomic), otherwise a symlink-preserving copy. Only the caller
/// removes the original after a successful copy.
fn move_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(libc::EXDEV) => {
            copy_tree(src, dst)?;
            fs::remove_dir_all(src)
        }
        Err(error) => Err(error),
    }
}

/// Recursive copy that recreates symlinks as symlinks instead of following them:
/// a `node_modules/.bin` link must stay a link, not turn into a copy of its target.
fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        let target = fs::read_link(src)?;
        std::os::unix::fs::symlink(target, dst)?;
        return Ok(());
    }
    if meta.is_dir() {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
        return Ok(());
    }
    fs::copy(src, dst)?;
    Ok(())
}

fn append_manifest(base: &Path, entry: &TrashEntry) {
    let Ok(text) = serde_json::to_string(entry) else {
        return;
    };
    let path = manifest_path_in(base);
    ensure_parent(&path);
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{text}");
    }
}

/// Moves one directory into the trash store and records it. The original only
/// disappears after the payload is fully inside the store.
pub fn move_to_trash(path: &Path, bytes: u64, source: &str) -> std::io::Result<TrashEntry> {
    move_to_trash_in(&trash_dir(), path, bytes, source)
}

fn move_to_trash_in(
    base: &Path,
    path: &Path,
    bytes: u64,
    source: &str,
) -> std::io::Result<TrashEntry> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "item".to_string());
    let id = format!("{}-{}", now_ms(), short_hash(path));
    let stored = format!("{id}-{name}");
    let dest = items_dir_in(base).join(&stored);
    if dest.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("trash item already exists: {stored}"),
        ));
    }
    fs::create_dir_all(items_dir_in(base))?;
    move_tree(path, &dest)?;
    let entry = TrashEntry {
        id,
        stored,
        original_path: path.display().to_string(),
        trashed_at: now_ms(),
        bytes,
        source: source.to_string(),
    };
    append_manifest(base, &entry);
    Ok(entry)
}

/// Every recorded entry whose payload is still in the store, newest first.
pub fn list() -> Vec<TrashEntry> {
    list_in(&trash_dir())
}

fn list_in(base: &Path) -> Vec<TrashEntry> {
    let Ok(text) = fs::read_to_string(manifest_path_in(base)) else {
        return Vec::new();
    };
    let mut entries: Vec<TrashEntry> = text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .filter(|entry: &TrashEntry| items_dir_in(base).join(&entry.stored).exists())
        .collect();
    entries.reverse();
    entries
}

/// Total bytes still sitting in the trash.
pub fn total_bytes() -> u64 {
    list().iter().map(|entry| entry.bytes).sum()
}

/// Finds one entry by id, id prefix, or "latest".
fn find_entry(entries: &[TrashEntry], reference: &str) -> Option<TrashEntry> {
    if reference == "latest" {
        return entries.first().cloned();
    }
    let matches: Vec<&TrashEntry> = entries
        .iter()
        .filter(|entry| entry.id == reference || entry.id.starts_with(reference))
        .collect();
    match matches.len() {
        1 => Some(matches[0].clone()),
        _ => None,
    }
}

/// Puts a trashed item back where it came from. Refuses to overwrite anything:
/// a path that appeared since the removal is left untouched.
pub fn restore(reference: &str) -> std::io::Result<PathBuf> {
    restore_in(&trash_dir(), reference)
}

fn restore_in(base: &Path, reference: &str) -> std::io::Result<PathBuf> {
    let entries = list_in(base);
    let Some(entry) = find_entry(&entries, reference) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no trash item matches '{reference}'"),
        ));
    };
    let dest = PathBuf::from(&entry.original_path);
    if dest.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("refusing to overwrite existing path: {}", dest.display()),
        ));
    }
    ensure_parent(&dest);
    move_tree(&items_dir_in(base).join(&entry.stored), &dest)?;
    Ok(dest)
}

/// Permanently deletes trashed items, optionally only those older than
/// `older_than_days`. Returns (removed, freed, errors).
pub fn empty(older_than_days: Option<i64>) -> (usize, u64, Vec<String>) {
    empty_in(&trash_dir(), older_than_days)
}

fn empty_in(base: &Path, older_than_days: Option<i64>) -> (usize, u64, Vec<String>) {
    let cutoff = older_than_days.map(|days| now_ms() - days.saturating_mul(86_400_000));
    let mut removed = 0usize;
    let mut freed = 0u64;
    let mut errors = Vec::new();
    for entry in list_in(base) {
        if let Some(cutoff) = cutoff {
            if entry.trashed_at > cutoff {
                continue;
            }
        }
        let path = items_dir_in(base).join(&entry.stored);
        match fs::remove_dir_all(&path) {
            Ok(()) => {
                removed += 1;
                freed += entry.bytes;
            }
            Err(error) => errors.push(format!("{}: {error}", path.display())),
        }
    }
    (removed, freed, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled_dir(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(dir.join("nested")).unwrap();
        fs::write(dir.join("nested/blob.bin"), "x".repeat(2048)).unwrap();
        dir
    }

    #[test]
    fn trash_and_restore_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("trash");
        let work = tmp.path().join("work");
        fs::create_dir_all(&work).unwrap();
        let dir = filled_dir(&work, "target");

        let entry = move_to_trash_in(&base, &dir, 2048, "clean").unwrap();
        assert!(!dir.exists());
        assert!(items_dir_in(&base).join(&entry.stored).exists());

        let listed = list_in(&base);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].original_path, dir.display().to_string());

        let restored = restore_in(&base, "latest").unwrap();
        assert_eq!(restored, dir);
        assert!(dir.join("nested/blob.bin").exists());
        assert!(list_in(&base).is_empty());
    }

    #[test]
    fn restore_refuses_to_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("trash");
        let work = tmp.path().join("work");
        fs::create_dir_all(&work).unwrap();
        let dir = filled_dir(&work, "node_modules");

        move_to_trash_in(&base, &dir, 2048, "projects").unwrap();
        fs::create_dir_all(&dir).unwrap();

        let error = restore_in(&base, "latest").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        // the payload stays in the trash, nothing is lost
        assert_eq!(list_in(&base).len(), 1);
    }

    #[test]
    fn restore_resolves_an_id_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("trash");
        let work = tmp.path().join("work");
        fs::create_dir_all(&work).unwrap();
        let dir = filled_dir(&work, "target");

        let entry = move_to_trash_in(&base, &dir, 2048, "clean").unwrap();
        let prefix = &entry.id[..12];
        assert!(restore_in(&base, prefix).is_ok());
    }

    #[test]
    fn empty_removes_everything_and_reports_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("trash");
        let work = tmp.path().join("work");
        fs::create_dir_all(&work).unwrap();
        filled_dir(&work, "a");
        filled_dir(&work, "b");

        move_to_trash_in(&base, &work.join("a"), 100, "clean").unwrap();
        move_to_trash_in(&base, &work.join("b"), 200, "clean").unwrap();

        let (removed, freed, errors) = empty_in(&base, None);
        assert_eq!(removed, 2);
        assert_eq!(freed, 300);
        assert!(errors.is_empty());
        assert!(list_in(&base).is_empty());
    }

    #[test]
    fn empty_with_age_filter_keeps_fresh_items() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("trash");
        let work = tmp.path().join("work");
        fs::create_dir_all(&work).unwrap();
        let dir = filled_dir(&work, "fresh");

        move_to_trash_in(&base, &dir, 100, "clean").unwrap();
        let (removed, _, _) = empty_in(&base, Some(7));
        assert_eq!(removed, 0);
        assert_eq!(list_in(&base).len(), 1);
    }

    #[test]
    fn a_manifest_line_for_a_gone_payload_is_not_listed() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("trash");
        let work = tmp.path().join("work");
        fs::create_dir_all(&work).unwrap();
        let dir = filled_dir(&work, "target");

        let entry = move_to_trash_in(&base, &dir, 100, "clean").unwrap();
        fs::remove_dir_all(items_dir_in(&base).join(&entry.stored)).unwrap();
        assert!(list_in(&base).is_empty());
    }

    #[test]
    fn copy_tree_keeps_symlinks_as_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("real.txt"), "data").unwrap();
        std::os::unix::fs::symlink("real.txt", src.join("link.txt")).unwrap();

        let dst = tmp.path().join("dst");
        copy_tree(&src, &dst).unwrap();
        let meta = fs::symlink_metadata(dst.join("link.txt")).unwrap();
        assert!(meta.file_type().is_symlink());
        assert_eq!(
            fs::read_link(dst.join("link.txt")).unwrap(),
            PathBuf::from("real.txt")
        );
    }
}

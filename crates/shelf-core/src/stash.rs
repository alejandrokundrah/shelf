//! The stash: a persistent holding area for files and folders.
//!
//! When no paired device is reachable, things dropped on the shelf are kept here instead of
//! being sent. Unlike received items they never expire and they survive restarts, so the shelf
//! works as a place to park files and drag them out again later.
//!
//! Each item is *copied* into `<root>/<id>/<original name>`. Copying (rather than pointing at
//! the original) means the stash keeps working if the original is moved, renamed or deleted,
//! and the original file name is preserved when the item is dragged back out. An index file
//! remembers what is stashed.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const INDEX_FILE: &str = "index.json";
/// Folders nested deeper than this are refused. It stops runaway recursion, for example
/// through Windows junctions that `symlink_metadata` does not report as links.
const MAX_DEPTH: usize = 64;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct StashEntry {
    /// 16 lowercase hex characters. Also the name of the folder that holds the copy.
    pub id: String,
    /// The original file or folder name, as shown to the user and used when dragging out.
    pub name: String,
    /// Where the stashed copy lives.
    pub path: PathBuf,
    pub is_dir: bool,
    /// Total bytes (for a folder, the sum of its files).
    pub size: u64,
    /// When it was stashed, in seconds since the Unix epoch.
    pub added: u64,
}

impl StashEntry {
    pub fn is_image(&self) -> bool {
        !self.is_dir
            && self.path.extension().is_some_and(|ext| {
                ["png", "jpg", "jpeg", "gif", "webp", "bmp"]
                    .iter()
                    .any(|known| ext.eq_ignore_ascii_case(known))
            })
    }
}

#[derive(Serialize, Deserialize)]
struct Index {
    version: u32,
    entries: Vec<StashEntry>,
}

pub struct Stash {
    root: PathBuf,
    /// Newest first.
    entries: Mutex<Vec<StashEntry>>,
}

impl Stash {
    /// Opens (creating if needed) the stash stored under `root`.
    ///
    /// Anything the index lists that is gone from disk is dropped, a damaged index is set
    /// aside rather than trusted, and leftovers from an interrupted copy are cleaned up.
    pub fn open(root: impl Into<PathBuf>) -> Stash {
        let root = root.into();
        let _ = fs::create_dir_all(&root);
        let mut entries = read_index(&root);
        entries.retain(|entry| entry.path.exists() && entry_is_inside(&root, entry));
        let stash = Stash {
            root,
            entries: Mutex::new(entries),
        };
        stash.sweep_leftovers();
        let _ = stash.save(&stash.entries.lock().unwrap());
        stash
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Everything currently stashed, newest first.
    pub fn entries(&self) -> Vec<StashEntry> {
        self.entries.lock().unwrap().clone()
    }

    /// True if `path` is the stash's own storage, or inside it. Dropping an item that was just
    /// dragged out back onto the shelf must not stash a copy of a copy.
    pub fn contains_path(&self, path: &Path) -> bool {
        let canonical = |p: &Path| fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        canonical(path).starts_with(canonical(&self.root))
    }

    /// Copies a file or folder into the stash. This can take a while for big folders, so the
    /// copy happens without holding any lock and the entry only appears once it is complete.
    pub fn add(&self, source: &Path) -> Result<StashEntry> {
        if self.contains_path(source) {
            bail!("that item is already in the stash");
        }
        // Follows a top-level link, so dropping a shortcut-style link stashes what it points at.
        let meta = fs::metadata(source)
            .with_context(|| format!("cannot read {}", source.display()))?;
        let name = source
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| "item".to_string());

        let id = format!("{:016x}", rand::random::<u64>());
        let staging = self.root.join(format!("{id}.tmp"));
        let final_dir = self.root.join(&id);
        fs::create_dir_all(&staging)?;

        let copied = (|| -> Result<u64> {
            let target = staging.join(&name);
            if meta.is_dir() {
                copy_dir(source, &target, 0)
            } else {
                Ok(fs::copy(source, &target)
                    .with_context(|| format!("cannot copy {}", source.display()))?)
            }
        })();
        let size = match copied {
            Ok(size) => size,
            Err(error) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(error);
            }
        };
        if let Err(error) = fs::rename(&staging, &final_dir) {
            let _ = fs::remove_dir_all(&staging);
            return Err(anyhow!("cannot finish stashing {name}: {error}"));
        }

        let entry = StashEntry {
            id,
            path: final_dir.join(&name),
            name,
            is_dir: meta.is_dir(),
            size,
            added: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        };
        let mut entries = self.entries.lock().unwrap();
        entries.insert(0, entry.clone());
        if let Err(error) = self.save(&entries) {
            // Keep the item usable this session even if the index could not be written.
            eprintln!("stash index not saved: {error}");
        }
        Ok(entry)
    }

    /// Removes one item and its stored copy. Returns false if there was no such item.
    pub fn remove(&self, id: &str) -> bool {
        let removed = {
            let mut entries = self.entries.lock().unwrap();
            let Some(position) = entries.iter().position(|e| e.id == id) else {
                return false;
            };
            let entry = entries.remove(position);
            let _ = self.save(&entries);
            entry
        };
        self.delete_entry_data(&removed);
        true
    }

    /// Removes everything.
    pub fn clear(&self) {
        let removed = {
            let mut entries = self.entries.lock().unwrap();
            let removed = std::mem::take(&mut *entries);
            let _ = self.save(&entries);
            removed
        };
        for entry in &removed {
            self.delete_entry_data(entry);
        }
    }

    fn delete_entry_data(&self, entry: &StashEntry) {
        // Delete the entry's own folder (found through the stored path, never by joining the
        // caller's id), and only if it really is inside the stash.
        if let Some(dir) = entry.path.parent() {
            if dir.parent() == Some(self.root.as_path()) {
                let _ = fs::remove_dir_all(dir);
            }
        }
    }

    fn save(&self, entries: &[StashEntry]) -> Result<()> {
        let index = Index {
            version: 1,
            entries: entries.to_vec(),
        };
        let json = serde_json::to_vec_pretty(&index)?;
        let temp = self.root.join(format!("{INDEX_FILE}.tmp"));
        fs::write(&temp, json)?;
        fs::rename(&temp, self.root.join(INDEX_FILE))?;
        Ok(())
    }

    /// Removes `*.tmp` staging folders from an interrupted copy, and item folders that the
    /// index no longer lists. Only folders named exactly like ours are touched.
    fn sweep_leftovers(&self) {
        let known: Vec<PathBuf> = self
            .entries
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| e.path.parent().map(Path::to_path_buf))
            .collect();
        let Ok(read) = fs::read_dir(&self.root) else {
            return;
        };
        for item in read.flatten() {
            let path = item.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let stem = name.strip_suffix(".tmp").unwrap_or(name);
            let ours = stem.len() == 16 && stem.chars().all(|c| c.is_ascii_hexdigit());
            if ours && path.is_dir() && (name.ends_with(".tmp") || !known.contains(&path)) {
                let _ = fs::remove_dir_all(&path);
            }
        }
    }
}

fn read_index(root: &Path) -> Vec<StashEntry> {
    let path = root.join(INDEX_FILE);
    let Ok(bytes) = fs::read(&path) else {
        return Vec::new();
    };
    match serde_json::from_slice::<Index>(&bytes) {
        Ok(index) => index.entries,
        Err(_) => {
            // Never silently discard a damaged index: keep it so it can be inspected.
            let _ = fs::rename(&path, root.join(format!("{INDEX_FILE}.bad")));
            Vec::new()
        }
    }
}

/// An index entry must point at `<root>/<16 hex id>/<name>` and nowhere else, so a edited or
/// corrupted index can never make the stash read or delete outside its own folder.
fn entry_is_inside(root: &Path, entry: &StashEntry) -> bool {
    let id_ok = entry.id.len() == 16 && entry.id.chars().all(|c| c.is_ascii_hexdigit());
    let Some(dir) = entry.path.parent() else {
        return false;
    };
    id_ok && dir == root.join(&entry.id) && entry.path.file_name().is_some()
}

/// Copies a folder recursively and returns the total bytes. Links inside the folder are
/// skipped rather than followed, so a link cannot pull in files from elsewhere or loop.
fn copy_dir(from: &Path, to: &Path, depth: usize) -> Result<u64> {
    if depth > MAX_DEPTH {
        bail!("folders nested more than {MAX_DEPTH} levels deep are not supported");
    }
    fs::create_dir_all(to)?;
    let mut total = 0;
    for child in fs::read_dir(from).with_context(|| format!("cannot read {}", from.display()))? {
        let child = child?;
        let kind = child.file_type()?;
        let destination = to.join(child.file_name());
        if kind.is_symlink() {
            continue;
        } else if kind.is_dir() {
            total += copy_dir(&child.path(), &destination, depth + 1)?;
        } else if kind.is_file() {
            total += fs::copy(child.path(), &destination)
                .with_context(|| format!("cannot copy {}", child.path().display()))?;
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "shelf-stash-{}-{:08x}",
                std::process::id(),
                rand::random::<u32>()
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn file(&self, relative: &str, contents: &[u8]) -> PathBuf {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
            path
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn stash_in(tmp: &TempDir) -> Stash {
        Stash::open(tmp.0.join("stash"))
    }

    #[test]
    fn a_file_is_copied_and_keeps_its_name() {
        let tmp = TempDir::new();
        let source = tmp.file("src/Holiday photo.jpg", b"jpeg bytes");
        let stash = stash_in(&tmp);

        let entry = stash.add(&source).unwrap();
        assert_eq!(entry.name, "Holiday photo.jpg");
        assert_eq!(entry.size, 10);
        assert!(!entry.is_dir && entry.is_image());
        assert_eq!(entry.path.file_name().unwrap(), "Holiday photo.jpg");
        assert_eq!(fs::read(&entry.path).unwrap(), b"jpeg bytes");
        assert_eq!(stash.entries(), vec![entry]);

        // It is a copy: the stash keeps working when the original is deleted.
        fs::remove_file(&source).unwrap();
        assert!(stash.entries()[0].path.exists());
    }

    #[test]
    fn a_folder_is_copied_recursively() {
        let tmp = TempDir::new();
        tmp.file("project/readme.txt", b"hello");
        tmp.file("project/src/deep/lib.rs", b"fn main() {}");
        fs::create_dir_all(tmp.0.join("project/empty")).unwrap();
        let stash = stash_in(&tmp);

        let entry = stash.add(&tmp.0.join("project")).unwrap();
        assert!(entry.is_dir && !entry.is_image());
        assert_eq!(entry.name, "project");
        assert_eq!(entry.size, 5 + 12);
        assert_eq!(fs::read(entry.path.join("readme.txt")).unwrap(), b"hello");
        assert_eq!(fs::read(entry.path.join("src/deep/lib.rs")).unwrap(), b"fn main() {}");
        assert!(entry.path.join("empty").is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn links_inside_a_folder_are_skipped_not_followed() {
        let tmp = TempDir::new();
        let secret = tmp.file("outside/secret.txt", b"do not copy");
        tmp.file("folder/ok.txt", b"ok");
        std::os::unix::fs::symlink(&secret, tmp.0.join("folder/link.txt")).unwrap();
        std::os::unix::fs::symlink(tmp.0.join("folder"), tmp.0.join("folder/loop")).unwrap();
        let stash = stash_in(&tmp);

        let entry = stash.add(&tmp.0.join("folder")).unwrap();
        assert!(entry.path.join("ok.txt").exists());
        assert!(!entry.path.join("link.txt").exists());
        assert!(!entry.path.join("loop").exists());
        assert_eq!(entry.size, 2);
    }

    #[test]
    fn the_same_file_twice_makes_two_independent_entries() {
        let tmp = TempDir::new();
        let source = tmp.file("a.txt", b"same");
        let stash = stash_in(&tmp);
        let first = stash.add(&source).unwrap();
        let second = stash.add(&source).unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(first.name, second.name);
        // Newest first.
        assert_eq!(stash.entries()[0].id, second.id);
        assert!(stash.remove(&second.id));
        assert!(first.path.exists(), "removing one must not touch the other");
    }

    #[test]
    fn entries_survive_a_restart_in_order() {
        let tmp = TempDir::new();
        let a = tmp.file("a.txt", b"a");
        let b = tmp.file("b.txt", b"b");
        let (first, second) = {
            let stash = stash_in(&tmp);
            (stash.add(&a).unwrap(), stash.add(&b).unwrap())
        };
        let reopened = stash_in(&tmp);
        assert_eq!(reopened.entries(), vec![second, first]);
    }

    #[test]
    fn entries_whose_data_vanished_are_dropped_on_open() {
        let tmp = TempDir::new();
        let source = tmp.file("a.txt", b"a");
        let entry = stash_in(&tmp).add(&source).unwrap();
        fs::remove_dir_all(entry.path.parent().unwrap()).unwrap();
        assert!(stash_in(&tmp).entries().is_empty());
    }

    #[test]
    fn a_damaged_index_is_kept_aside_and_the_stash_starts_empty() {
        let tmp = TempDir::new();
        fs::create_dir_all(tmp.0.join("stash")).unwrap();
        fs::write(tmp.0.join("stash").join(INDEX_FILE), b"{ not json").unwrap();
        let stash = stash_in(&tmp);
        assert!(stash.entries().is_empty());
        assert!(tmp.0.join("stash").join("index.json.bad").exists());
        // And it is usable afterwards.
        stash.add(&tmp.file("a.txt", b"a")).unwrap();
        assert_eq!(stash_in(&tmp).entries().len(), 1);
    }

    #[test]
    fn an_index_cannot_point_outside_the_stash() {
        let tmp = TempDir::new();
        let outside = tmp.file("precious/keep.txt", b"keep");
        let root = tmp.0.join("stash");
        fs::create_dir_all(&root).unwrap();
        let evil = Index {
            version: 1,
            entries: vec![
                StashEntry {
                    id: "0123456789abcdef".into(),
                    name: "keep.txt".into(),
                    path: outside.clone(),
                    is_dir: false,
                    size: 4,
                    added: 0,
                },
                StashEntry {
                    id: "../precious".into(),
                    name: "keep.txt".into(),
                    path: root.join("../precious/keep.txt"),
                    is_dir: false,
                    size: 4,
                    added: 0,
                },
            ],
        };
        fs::write(root.join(INDEX_FILE), serde_json::to_vec(&evil).unwrap()).unwrap();
        let stash = Stash::open(&root);
        assert!(stash.entries().is_empty());
        stash.clear();
        assert!(outside.exists(), "nothing outside the stash may ever be deleted");
    }

    #[test]
    fn remove_and_clear_delete_the_stored_data() {
        let tmp = TempDir::new();
        let stash = stash_in(&tmp);
        let one = stash.add(&tmp.file("one.txt", b"1")).unwrap();
        let two = stash.add(&tmp.file("two.txt", b"2")).unwrap();
        assert!(stash.remove(&one.id));
        assert!(!one.path.exists() && !one.path.parent().unwrap().exists());
        assert!(!stash.remove(&one.id), "removing twice reports nothing was there");
        assert!(!stash.remove("nope"));
        assert!(two.path.exists());
        stash.clear();
        assert!(stash.entries().is_empty() && !two.path.exists());
        assert!(stash_in(&tmp).entries().is_empty());
    }

    #[test]
    fn something_already_in_the_stash_cannot_be_added_again() {
        let tmp = TempDir::new();
        let stash = stash_in(&tmp);
        let entry = stash.add(&tmp.file("a.txt", b"a")).unwrap();
        // This is what happens when an item is dragged out and dropped back on the shelf.
        assert!(stash.contains_path(&entry.path));
        assert!(stash.add(&entry.path).is_err());
        assert!(stash.add(entry.path.parent().unwrap()).is_err());
        assert_eq!(stash.entries().len(), 1);
        assert!(!stash.contains_path(&tmp.0.join("a.txt")));
    }

    #[test]
    fn a_missing_source_fails_cleanly_and_leaves_no_trace() {
        let tmp = TempDir::new();
        let stash = stash_in(&tmp);
        assert!(stash.add(&tmp.0.join("does-not-exist")).is_err());
        assert!(stash.entries().is_empty());
        let leftovers: Vec<_> = fs::read_dir(stash.root())
            .unwrap()
            .flatten()
            .filter(|e| e.path().is_dir())
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn interrupted_copies_are_cleaned_up_on_open_but_other_files_are_left_alone() {
        let tmp = TempDir::new();
        let root = tmp.0.join("stash");
        fs::create_dir_all(root.join("0123456789abcdef.tmp")).unwrap();
        fs::create_dir_all(root.join("fedcba9876543210")).unwrap(); // not in the index
        fs::create_dir_all(root.join("my own notes")).unwrap(); // not ours
        fs::write(root.join("notes.txt"), b"x").unwrap();
        let _ = Stash::open(&root);
        assert!(!root.join("0123456789abcdef.tmp").exists());
        assert!(!root.join("fedcba9876543210").exists());
        assert!(root.join("my own notes").exists());
        assert!(root.join("notes.txt").exists());
    }

    #[test]
    fn very_deep_folders_are_refused_instead_of_recursing_forever() {
        let tmp = TempDir::new();
        let mut deep = tmp.0.join("deep");
        for _ in 0..(MAX_DEPTH + 5) {
            deep = deep.join("d");
        }
        fs::create_dir_all(&deep).unwrap();
        let stash = stash_in(&tmp);
        assert!(stash.add(&tmp.0.join("deep")).is_err());
        assert!(stash.entries().is_empty());
    }

    #[test]
    fn a_large_file_is_copied_intact() {
        let tmp = TempDir::new();
        let data: Vec<u8> = (0..5_000_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8).collect();
        let source = tmp.file("big.bin", &data);
        let entry = stash_in(&tmp).add(&source).unwrap();
        assert_eq!(entry.size, data.len() as u64);
        assert_eq!(fs::read(&entry.path).unwrap(), data);
    }
}

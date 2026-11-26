//! The undo log. Every applied plan is one [`JournalEntry`] made of [`Step`]s
//! that can be walked backwards. Originals replaced by processed images are
//! kept in `trash/<entry id>/` until the entry ages out of the journal.

use super::{move_file, unique_path};
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const MAX_ENTRIES: usize = 300;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Step {
    /// A folder the action had to create. Removed on undo if still empty.
    CreatedDir(PathBuf),
    Move { from: PathBuf, to: PathBuf },
    /// The original of a processed image, parked in the trash.
    Trash { original: PathBuf, stored: PathBuf },
    /// A file the action wrote. Deleted on undo.
    Create(PathBuf),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub id: u64,
    /// Unix seconds.
    pub time: i64,
    pub rule: String,
    pub source: PathBuf,
    pub dest: PathBuf,
    pub steps: Vec<Step>,
    pub undone: bool,
}

impl JournalEntry {
    /// Local date and time, e.g. "Nov 26 05:12".
    pub fn when(&self) -> String {
        use chrono::TimeZone;
        chrono::Local
            .timestamp_opt(self.time, 0)
            .single()
            .map(|t| t.format("%b %d %H:%M").to_string())
            .unwrap_or_default()
    }
}

pub struct Journal {
    dir: PathBuf,
    entries: Vec<JournalEntry>,
    last_id: u64,
}

impl Journal {
    pub fn load(dir: &Path) -> Journal {
        let entries: Vec<JournalEntry> = std::fs::read(dir.join("journal.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let last_id = entries.iter().map(|e| e.id).max().unwrap_or(0);
        Journal { dir: dir.to_path_buf(), entries, last_id }
    }

    pub fn entries(&self) -> &[JournalEntry] {
        &self.entries
    }

    pub fn trash_dir(&self) -> PathBuf {
        self.dir.join("trash")
    }

    /// Milliseconds since the epoch, bumped if needed so ids never repeat.
    pub fn next_id(&mut self) -> u64 {
        let now = chrono::Local::now().timestamp_millis().max(0) as u64;
        self.last_id = now.max(self.last_id + 1);
        self.last_id
    }

    pub fn push(&mut self, entry: JournalEntry) {
        self.entries.push(entry);
        while self.entries.len() > MAX_ENTRIES {
            let old = self.entries.remove(0);
            self.purge_trash(old.id);
        }
        self.save();
    }

    fn save(&self) {
        if let Ok(bytes) = serde_json::to_vec_pretty(&self.entries) {
            let _ = std::fs::create_dir_all(&self.dir);
            let _ = std::fs::write(self.dir.join("journal.json"), bytes);
        }
    }

    fn purge_trash(&self, id: u64) {
        let _ = std::fs::remove_dir_all(self.trash_dir().join(id.to_string()));
    }

    /// Forgets all history and deletes the trashed originals. This makes
    /// every earlier action permanent.
    pub fn clear(&mut self) {
        self.entries.clear();
        let _ = std::fs::remove_dir_all(self.trash_dir());
        self.save();
    }

    /// Reverses one entry. Checks first that everything it needs is still
    /// there, so a failed undo leaves the disk as it found it.
    pub fn undo(&mut self, id: u64) -> Result<()> {
        let entry = self
            .entries
            .iter()
            .find(|e| e.id == id)
            .cloned()
            .ok_or_else(|| anyhow!("that action is no longer in the history"))?;
        if entry.undone {
            bail!("already undone");
        }
        for step in &entry.steps {
            match step {
                Step::Move { to, .. } if !to.exists() => {
                    bail!("{} has been moved or deleted since", to.display())
                }
                Step::Trash { stored, .. } if !stored.exists() => {
                    bail!("the saved original is gone ({})", stored.display())
                }
                _ => {}
            }
        }
        for step in entry.steps.iter().rev() {
            match step {
                Step::Create(path) => {
                    let _ = std::fs::remove_file(path);
                }
                Step::Trash { original, stored } => {
                    let target = if original.exists() { unique_path(original) } else { original.clone() };
                    move_file(stored, &target)?;
                }
                Step::Move { from, to } => {
                    let target = if from.exists() { unique_path(from) } else { from.clone() };
                    move_file(to, &target)?;
                }
                Step::CreatedDir(dir) => {
                    let _ = std::fs::remove_dir(dir); // fails (harmlessly) if not empty
                }
            }
        }
        self.purge_trash(id);
        if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
            e.undone = true;
        }
        self.save();
        Ok(())
    }
}

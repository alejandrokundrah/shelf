//! Polling folder watcher. Polling (not OS notifications) keeps it simple and
//! behaves the same on network drives. A file is only acted on once its size
//! and modified time have stopped changing for `settle`, so half-downloaded
//! files are left alone.

use super::{apply_plan, plan_file, Config, FileInfo, Journal, JournalEntry, Plan};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

#[derive(Debug)]
pub enum OrganizerEvent {
    /// Preview mode: a new file was planned but nothing was touched.
    Proposed(Plan),
    /// Auto mode: the plan was carried out.
    Applied(JournalEntry),
    Failed { path: PathBuf, error: String },
}

/// Temporary, hidden and system files the organizer never touches.
pub fn is_ignored(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    if name.starts_with('.') || name.starts_with("~$") {
        return true;
    }
    if matches!(name.as_str(), "desktop.ini" | "thumbs.db") {
        return true;
    }
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    matches!(
        ext.as_str(),
        "crdownload" | "part" | "partial" | "tmp" | "temp" | "download" | "opdownload" | "!ut"
    )
}

pub struct Watcher {
    stop: Arc<AtomicBool>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

struct Pending {
    size: u64,
    modified: Option<SystemTime>,
    since: Instant,
}

fn list_files(folder: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && !is_ignored(p))
        .collect()
}

pub fn spawn(
    config: Arc<Mutex<Config>>,
    journal: Arc<Mutex<Journal>>,
    tx: mpsc::Sender<OrganizerEvent>,
    wake: Arc<dyn Fn() + Send + Sync>,
    poll: Duration,
    settle: Duration,
) -> Watcher {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    std::thread::spawn(move || {
        let mut folder: Option<PathBuf> = None;
        // Files that were already there (or already handled) are never re-planned.
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let mut pending: HashMap<PathBuf, Pending> = HashMap::new();

        while !flag.load(Ordering::SeqCst) {
            std::thread::sleep(poll);
            if flag.load(Ordering::SeqCst) {
                break;
            }
            let cfg = config.lock().unwrap().clone();
            let Some(dir) = cfg.watch_folder.clone() else { continue };

            let files = list_files(&dir);
            if folder.as_ref() != Some(&dir) {
                // First look at this folder: only files that arrive *after* now count.
                folder = Some(dir.clone());
                seen = files.iter().cloned().collect();
                pending.clear();
                continue;
            }

            let present: HashSet<&PathBuf> = files.iter().collect();
            pending.retain(|p, _| present.contains(p));
            seen.retain(|p| present.contains(p));

            let mut sent = false;
            for path in &files {
                if seen.contains(path) {
                    continue;
                }
                let Ok(meta) = std::fs::metadata(path) else { continue };
                let (size, modified) = (meta.len(), meta.modified().ok());
                let entry = pending
                    .entry(path.clone())
                    .or_insert(Pending { size, modified, since: Instant::now() });
                if entry.size != size || entry.modified != modified {
                    *entry = Pending { size, modified, since: Instant::now() };
                    continue;
                }
                if entry.since.elapsed() < settle {
                    continue;
                }
                pending.remove(path);
                seen.insert(path.clone());

                let Ok(info) = FileInfo::from_path(path) else { continue };
                let Some(plan) = plan_file(&info, &dir, &cfg.rules, 0) else { continue };
                let event = if cfg.auto_apply {
                    // Lock the journal only for the apply itself.
                    let result = apply_plan(&plan, &mut journal.lock().unwrap());
                    match result {
                        Ok(done) => {
                            seen.insert(done.dest.clone());
                            OrganizerEvent::Applied(done)
                        }
                        Err(e) => OrganizerEvent::Failed { path: path.clone(), error: e.to_string() },
                    }
                } else {
                    OrganizerEvent::Proposed(plan)
                };
                if tx.send(event).is_err() {
                    return; // the organizer was dropped
                }
                sent = true;
            }
            if sent {
                wake();
            }
        }
    });
    Watcher { stop }
}

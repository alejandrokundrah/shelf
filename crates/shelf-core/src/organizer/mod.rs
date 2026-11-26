//! Downloads organizer and batch image tool.
//!
//! A [`Rule`] is *conditions + actions*. Files reach the organizer two ways:
//! automatically (a polling watcher on a folder such as Downloads) or manually
//! (a batch of dropped files). Either way each file is turned into a [`Plan`]
//! first, so the UI can show a preview, and applying a plan writes a
//! [`JournalEntry`] that [`Journal::undo`] can reverse. Originals are never
//! deleted: image processing moves the original into the journal's trash.

mod imageops;
mod journal;
mod nlrule;
mod watcher;

pub use journal::{Journal, JournalEntry, Step};
pub use imageops::can_decode;
pub use nlrule::parse_rule;
pub use watcher::OrganizerEvent;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

// ---------------------------------------------------------------------------
// File categories
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Category {
    Images,
    Pdfs,
    Installers,
    Archives,
    Documents,
    Audio,
    Video,
    Other,
}

impl Category {
    pub const ALL: [Category; 8] = [
        Category::Images,
        Category::Pdfs,
        Category::Installers,
        Category::Archives,
        Category::Documents,
        Category::Audio,
        Category::Video,
        Category::Other,
    ];

    /// Also the name of the folder the default rules sort this category into.
    pub fn label(self) -> &'static str {
        match self {
            Category::Images => "Images",
            Category::Pdfs => "PDFs",
            Category::Installers => "Installers",
            Category::Archives => "Archives",
            Category::Documents => "Documents",
            Category::Audio => "Audio",
            Category::Video => "Video",
            Category::Other => "Other",
        }
    }

    pub fn of_ext(ext: &str) -> Category {
        match ext.to_ascii_lowercase().as_str() {
            "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "tif" | "tiff" | "heic" | "svg"
            | "ico" | "avif" => Category::Images,
            "pdf" => Category::Pdfs,
            "exe" | "msi" | "msix" | "appx" | "dmg" | "pkg" | "apk" | "deb" | "rpm" => {
                Category::Installers
            }
            "zip" | "rar" | "7z" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "iso" => {
                Category::Archives
            }
            "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "rtf" | "odt" | "ods"
            | "odp" | "csv" | "md" => Category::Documents,
            "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a" | "wma" => Category::Audio,
            "mp4" | "mkv" | "mov" | "avi" | "webm" | "wmv" | "flv" => Category::Video,
            _ => Category::Other,
        }
    }
}

// ---------------------------------------------------------------------------
// Rules
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageFormat {
    Png,
    Jpeg,
    Webp,
}

impl ImageFormat {
    pub fn ext(self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpg",
            ImageFormat::Webp => "webp",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ImageFormat::Png => "PNG",
            ImageFormat::Jpeg => "JPG",
            ImageFormat::Webp => "WebP",
        }
    }

    pub fn from_ext(ext: &str) -> Option<ImageFormat> {
        match ext.to_ascii_lowercase().as_str() {
            "png" => Some(ImageFormat::Png),
            "jpg" | "jpeg" => Some(ImageFormat::Jpeg),
            "webp" => Some(ImageFormat::Webp),
            _ => None,
        }
    }
}

/// A rule matches when *all* of its conditions hold. A rule with no
/// conditions matches nothing, so a half-written rule can never sweep up
/// every file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Condition {
    Category(Category),
    /// Lowercase, no leading dot.
    Extension(String),
    /// Case-insensitive match on the file name.
    NameContains(String),
    SizeOver(u64),
    SizeUnder(u64),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Action {
    /// Relative paths resolve against the watched folder; a leading
    /// `Pictures`, `Documents`, `Desktop`, `Music`, `Videos` or `Downloads`
    /// means the user's own folder of that name.
    MoveTo(PathBuf),
    /// Template for the name without extension. Tokens: `{name}`, `{date}`
    /// (YYYY-MM-DD), `{n}` (position in the batch, 3 digits), `{ext}`. The
    /// extension is appended automatically unless the template uses `{ext}`.
    Rename(String),
    /// Shrink to fit inside the box (0 = no limit on that side). Never enlarges.
    Resize { max_width: u32, max_height: u32 },
    Convert(ImageFormat),
    /// 1-100. Affects JPEG; PNG and WebP are lossless, so they just use the
    /// strongest compression.
    Compress { quality: u8 },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub id: u64,
    pub name: String,
    pub enabled: bool,
    pub conditions: Vec<Condition>,
    pub actions: Vec<Action>,
}

impl Rule {
    pub fn matches(&self, info: &FileInfo) -> bool {
        self.enabled
            && !self.conditions.is_empty()
            && self.conditions.iter().all(|c| c.matches(info))
    }

    /// One-line plain-English summary, shown in the rules list.
    pub fn describe(&self) -> String {
        let when: Vec<String> = self.conditions.iter().map(Condition::describe).collect();
        let then: Vec<String> = self.actions.iter().map(Action::describe).collect();
        let when = if when.is_empty() {
            "never (no conditions)".to_string()
        } else {
            when.join(" and ")
        };
        let then = if then.is_empty() {
            "do nothing".to_string()
        } else {
            then.join(", then ")
        };
        format!("If {when}: {then}")
    }
}

impl Condition {
    pub fn matches(&self, info: &FileInfo) -> bool {
        match self {
            Condition::Category(c) => Category::of_ext(&info.ext) == *c,
            Condition::Extension(e) => {
                norm_ext(&info.ext) == norm_ext(e.trim_start_matches('.'))
            }
            Condition::NameContains(s) => info.name.to_lowercase().contains(&s.to_lowercase()),
            Condition::SizeOver(n) => info.size > *n,
            Condition::SizeUnder(n) => info.size < *n,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Condition::Category(c) => format!("it is in {}", c.label()),
            Condition::Extension(e) => format!("it is a .{} file", e.trim_start_matches('.')),
            Condition::NameContains(s) => format!("the name contains \"{s}\""),
            Condition::SizeOver(n) => format!("it is over {}", human_size(*n)),
            Condition::SizeUnder(n) => format!("it is under {}", human_size(*n)),
        }
    }
}

impl Action {
    pub fn describe(&self) -> String {
        match self {
            Action::MoveTo(p) => format!("move to {}", p.display()),
            Action::Rename(t) => format!("rename to {t}"),
            Action::Resize { max_width, max_height } => match (*max_width, *max_height) {
                (w, 0) => format!("resize to {w}px wide"),
                (0, h) => format!("resize to {h}px tall"),
                (w, h) => format!("resize to fit {w}×{h}"),
            },
            Action::Convert(f) => format!("convert to {}", f.label()),
            Action::Compress { quality } => format!("compress (quality {quality})"),
        }
    }
}

/// `jpeg` = `jpg`, `tiff` = `tif`, case-insensitive.
fn norm_ext(ext: &str) -> String {
    match ext.to_ascii_lowercase().as_str() {
        "jpeg" => "jpg".to_string(),
        "tiff" => "tif".to_string(),
        other => other.to_string(),
    }
}

/// The built-in "sort by file type" rules.
pub fn default_rules() -> Vec<Rule> {
    Category::ALL
        .iter()
        .filter(|c| **c != Category::Other)
        .enumerate()
        .map(|(i, c)| Rule {
            id: i as u64 + 1,
            name: format!("Sort {}", c.label().to_lowercase()),
            enabled: true,
            conditions: vec![Condition::Category(*c)],
            actions: vec![Action::MoveTo(PathBuf::from(c.label()))],
        })
        .collect()
}

pub fn next_rule_id(rules: &[Rule]) -> u64 {
    rules.iter().map(|r| r.id).max().unwrap_or(0) + 1
}

// ---------------------------------------------------------------------------
// Files, plans
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct FileInfo {
    pub path: PathBuf,
    pub name: String,
    pub stem: String,
    /// Lowercase, no dot; empty if the file has none.
    pub ext: String,
    pub size: u64,
}

impl FileInfo {
    pub fn from_path(path: &Path) -> Result<FileInfo> {
        let meta = std::fs::metadata(path)?;
        if !meta.is_file() {
            return Err(anyhow!("not a file: {}", path.display()));
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| anyhow!("no file name"))?;
        let stem = path
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| name.clone());
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        Ok(FileInfo { path: path.to_path_buf(), name, stem, ext, size: meta.len() })
    }
}

/// Image steps of a plan, in the order they are always applied.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImageOps {
    pub resize: Option<(u32, u32)>,
    pub convert: Option<ImageFormat>,
    pub compress: Option<u8>,
}

impl ImageOps {
    pub fn has_work(&self) -> bool {
        self.resize.is_some() || self.convert.is_some() || self.compress.is_some()
    }

    /// The format the result is written in: the requested one, else the
    /// source's own (PNG for source formats Shelf can read but not write).
    pub fn output_format(&self, src_ext: &str) -> ImageFormat {
        self.convert
            .or_else(|| ImageFormat::from_ext(src_ext))
            .unwrap_or(ImageFormat::Png)
    }
}

/// What the organizer *would* do to one file. Nothing has touched the disk yet.
#[derive(Clone, Debug)]
pub struct Plan {
    pub source: PathBuf,
    pub rule: String,
    /// Where the result should land. If something is already there at apply
    /// time, a numbered name is used instead.
    pub dest: PathBuf,
    pub ops: ImageOps,
    pub source_size: u64,
    /// Human-readable steps for the preview ("Convert to WebP", …).
    pub summary: Vec<String>,
}

/// Plans `info` with the first enabled rule that matches. `index` is the
/// file's position in the batch (for `{n}`). `None` means no rule matched or
/// the rule would change nothing.
pub fn plan_file(info: &FileInfo, base: &Path, rules: &[Rule], index: usize) -> Option<Plan> {
    let rule = rules.iter().find(|r| r.matches(info))?;
    build_plan(info, base, rule, index)
}

pub fn build_plan(info: &FileInfo, base: &Path, rule: &Rule, index: usize) -> Option<Plan> {
    let mut ops = ImageOps::default();
    let mut folder: Option<PathBuf> = None;
    let mut template: Option<String> = None;
    for action in &rule.actions {
        match action {
            Action::MoveTo(p) => folder = Some(p.clone()),
            Action::Rename(t) => template = Some(t.clone()),
            Action::Resize { max_width, max_height } => {
                ops.resize = Some((*max_width, *max_height))
            }
            Action::Convert(f) => ops.convert = Some(*f),
            Action::Compress { quality } => ops.compress = Some((*quality).clamp(1, 100)),
        }
    }

    let mut summary = Vec::new();
    if ops.has_work() && !imageops::can_decode(&info.ext) {
        summary.push("Skipped image steps (not a PNG, JPG, WebP, GIF or BMP)".to_string());
        ops = ImageOps::default();
    }
    if let Some((w, h)) = ops.resize {
        summary.push(Action::Resize { max_width: w, max_height: h }.describe());
    }
    if let Some(f) = ops.convert {
        summary.push(Action::Convert(f).describe());
    }
    if let Some(q) = ops.compress {
        summary.push(Action::Compress { quality: q }.describe());
    }

    let out_ext = if ops.has_work() {
        ops.output_format(&info.ext).ext().to_string()
    } else {
        info.ext.clone()
    };
    let file_name = match &template {
        Some(t) => {
            let expanded = expand_template(t, &info.stem, &out_ext, index);
            if t.contains("{ext}") || out_ext.is_empty() {
                expanded
            } else {
                format!("{expanded}.{out_ext}")
            }
        }
        None if out_ext.is_empty() => info.stem.clone(),
        None => format!("{}.{}", info.stem, out_ext),
    };
    let file_name = sanitize_file_name(&file_name);

    let dir = match &folder {
        Some(f) => resolve_dest(base, f),
        None => info.path.parent().map(Path::to_path_buf).unwrap_or_default(),
    };
    let dest = dir.join(&file_name);
    if !ops.has_work() && dest == info.path {
        return None;
    }
    if file_name != info.name {
        summary.push(format!("Rename to {file_name}"));
    }
    if Some(dir.as_path()) != info.path.parent() {
        summary.push(format!("Move to {}", dir.display()));
    }
    Some(Plan {
        source: info.path.clone(),
        rule: rule.name.clone(),
        dest,
        ops,
        source_size: info.size,
        summary,
    })
}

fn expand_template(template: &str, stem: &str, ext: &str, index: usize) -> String {
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    template
        .replace("{name}", stem)
        .replace("{date}", &date)
        .replace("{n}", &format!("{:03}", index + 1))
        .replace("{ext}", ext)
}

fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim().trim_end_matches('.').to_string();
    if cleaned.is_empty() {
        "file".to_string()
    } else {
        cleaned
    }
}

/// Resolves a rule's destination folder. Absolute paths are used as-is, a
/// leading well-known folder name maps to the user's real one, anything else
/// is relative to `base`.
pub fn resolve_dest(base: &Path, folder: &Path) -> PathBuf {
    if folder.is_absolute() {
        return folder.to_path_buf();
    }
    let mut comps = folder.components();
    if let Some(first) = comps.next() {
        let first = first.as_os_str().to_string_lossy().to_ascii_lowercase();
        let known = match first.as_str() {
            "pictures" => dirs::picture_dir(),
            "documents" => dirs::document_dir(),
            "desktop" => dirs::desktop_dir(),
            "music" => dirs::audio_dir(),
            "videos" => dirs::video_dir(),
            "downloads" => dirs::download_dir(),
            _ => None,
        };
        if let Some(root) = known {
            return root.join(comps.as_path());
        }
    }
    base.join(folder)
}

/// `name.ext` → `name (1).ext` → `name (2).ext` … until unused.
pub(crate) fn unique_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (1..)
        .map(|n| parent.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .expect("unbounded range")
}

pub fn human_size(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else if (value - value.round()).abs() < 0.05 {
        format!("{} {}", value.round() as u64, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Moves a file, copying when the destination is on another volume.
pub(crate) fn move_file(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to)?;
    std::fs::remove_file(from)?;
    Ok(())
}

fn missing_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut missing: Vec<PathBuf> =
        dir.ancestors().take_while(|d| !d.exists()).map(Path::to_path_buf).collect();
    missing.reverse();
    missing
}

/// Carries out a plan and records it in `journal` so it can be undone.
pub fn apply_plan(plan: &Plan, journal: &mut Journal) -> Result<JournalEntry> {
    // Encode first: if the image can't be read, nothing on disk has changed.
    let encoded = if plan.ops.has_work() {
        let ext = plan.source.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
        Some(imageops::process(&plan.source, &plan.ops, &ext)?.0)
    } else {
        None
    };

    let id = journal.next_id();
    let mut steps = Vec::new();
    let parent = plan.dest.parent().map(Path::to_path_buf).unwrap_or_default();
    let created = missing_dirs(&parent);
    std::fs::create_dir_all(&parent)?;
    steps.extend(created.into_iter().map(Step::CreatedDir));

    let dest = match encoded {
        Some(bytes) => {
            let in_place = plan.dest == plan.source;
            let dest = if in_place { plan.dest.clone() } else { unique_path(&plan.dest) };
            let name = plan.source.file_name().map(|n| n.to_os_string()).unwrap_or_default();
            let stored = journal.trash_dir().join(id.to_string()).join(name);
            move_file(&plan.source, &stored)?;
            if let Err(e) = std::fs::write(&dest, &bytes) {
                let _ = move_file(&stored, &plan.source);
                return Err(e.into());
            }
            steps.push(Step::Trash { original: plan.source.clone(), stored });
            steps.push(Step::Create(dest.clone()));
            dest
        }
        None => {
            let dest = unique_path(&plan.dest);
            move_file(&plan.source, &dest)?;
            steps.push(Step::Move { from: plan.source.clone(), to: dest.clone() });
            dest
        }
    };

    let entry = JournalEntry {
        id,
        time: chrono::Local::now().timestamp(),
        rule: plan.rule.clone(),
        source: plan.source.clone(),
        dest,
        steps,
        undone: false,
    };
    journal.push(entry.clone());
    Ok(entry)
}

// ---------------------------------------------------------------------------
// Config + Organizer (what the UI holds)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub watch_folder: Option<PathBuf>,
    /// Whether the watcher is running.
    pub watching: bool,
    /// `false` = preview mode: the watcher only proposes plans and waits.
    pub auto_apply: bool,
    pub rules: Vec<Rule>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            watch_folder: dirs::download_dir(),
            watching: false,
            auto_apply: false,
            rules: default_rules(),
        }
    }
}

pub struct Organizer {
    dir: PathBuf,
    config: Arc<Mutex<Config>>,
    journal: Arc<Mutex<Journal>>,
    tx: mpsc::Sender<OrganizerEvent>,
    rx: mpsc::Receiver<OrganizerEvent>,
    watcher: Option<watcher::Watcher>,
    wake: Arc<dyn Fn() + Send + Sync>,
    poll: Duration,
    settle: Duration,
}

impl Organizer {
    /// `dir` holds `config.json`, `journal.json` and the trash. The UI passes
    /// `data_dir().join("organizer")`.
    pub fn open(dir: PathBuf, wake: impl Fn() + Send + Sync + 'static) -> Organizer {
        Self::open_with(dir, wake, Duration::from_millis(1000), Duration::from_millis(2500))
    }

    pub fn open_with(
        dir: PathBuf,
        wake: impl Fn() + Send + Sync + 'static,
        poll: Duration,
        settle: Duration,
    ) -> Organizer {
        let _ = std::fs::create_dir_all(&dir);
        let config: Config = std::fs::read(dir.join("config.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let journal = Journal::load(&dir);
        let (tx, rx) = mpsc::channel();
        let mut organizer = Organizer {
            dir,
            config: Arc::new(Mutex::new(config)),
            journal: Arc::new(Mutex::new(journal)),
            tx,
            rx,
            watcher: None,
            wake: Arc::new(wake),
            poll,
            settle,
        };
        organizer.restart_watcher();
        organizer
    }

    pub fn config(&self) -> Config {
        self.config.lock().unwrap().clone()
    }

    /// Edits the config, saves it, and restarts the watcher to match.
    pub fn update_config(&mut self, edit: impl FnOnce(&mut Config)) {
        let restart = {
            let mut cfg = self.config.lock().unwrap();
            let before = (cfg.watching, cfg.watch_folder.clone());
            edit(&mut cfg);
            if let Ok(bytes) = serde_json::to_vec_pretty(&*cfg) {
                let _ = std::fs::write(self.dir.join("config.json"), bytes);
            }
            before != (cfg.watching, cfg.watch_folder.clone())
        };
        // The watcher re-reads rules and the preview/auto setting on every
        // poll, so only a different folder or on/off switch needs a restart
        // (a restart would forget files that are still settling).
        if restart {
            self.restart_watcher();
        }
    }

    fn restart_watcher(&mut self) {
        self.watcher = None; // dropping stops the old thread
        let cfg = self.config();
        if cfg.watching && cfg.watch_folder.is_some() {
            self.watcher = Some(watcher::spawn(
                self.config.clone(),
                self.journal.clone(),
                self.tx.clone(),
                self.wake.clone(),
                self.poll,
                self.settle,
            ));
        }
    }

    /// Events from the watcher since the last call.
    pub fn poll(&self) -> Vec<OrganizerEvent> {
        self.rx.try_iter().collect()
    }

    /// Plans for dropped files. Folders are walked recursively. `rules`
    /// defaults to the saved ones; `base` anchors relative `MoveTo` folders.
    pub fn plan_paths(&self, paths: &[PathBuf], rules: Option<&[Rule]>, base: &Path) -> Vec<Plan> {
        let saved;
        let rules = match rules {
            Some(r) => r,
            None => {
                saved = self.config().rules;
                &saved
            }
        };
        expand_paths(paths)
            .iter()
            .enumerate()
            .filter_map(|(i, p)| plan_file(&FileInfo::from_path(p).ok()?, base, rules, i))
            .collect()
    }

    /// Plans for every file directly inside the watched folder ("tidy up now").
    pub fn plan_existing(&self) -> Vec<Plan> {
        let cfg = self.config();
        let Some(folder) = cfg.watch_folder.clone() else { return Vec::new() };
        let mut files: Vec<PathBuf> = std::fs::read_dir(&folder)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && !watcher::is_ignored(p))
            .collect();
        files.sort();
        files
            .iter()
            .enumerate()
            .filter_map(|(i, p)| plan_file(&FileInfo::from_path(p).ok()?, &folder, &cfg.rules, i))
            .collect()
    }

    pub fn apply(&self, plan: &Plan) -> Result<JournalEntry> {
        apply_plan(plan, &mut self.journal.lock().unwrap())
    }

    /// Applies every plan; returns what succeeded and what failed (with why).
    pub fn apply_all(&self, plans: &[Plan]) -> (Vec<JournalEntry>, Vec<(PathBuf, String)>) {
        let mut done = Vec::new();
        let mut failed = Vec::new();
        for plan in plans {
            match self.apply(plan) {
                Ok(entry) => done.push(entry),
                Err(e) => failed.push((plan.source.clone(), e.to_string())),
            }
        }
        (done, failed)
    }

    /// Newest first.
    pub fn history(&self) -> Vec<JournalEntry> {
        let mut entries = self.journal.lock().unwrap().entries().to_vec();
        entries.reverse();
        entries
    }

    pub fn undo(&self, id: u64) -> Result<()> {
        self.journal.lock().unwrap().undo(id)
    }

    /// Undoes the newest action that has not been undone yet.
    pub fn undo_last(&self) -> Result<JournalEntry> {
        let mut journal = self.journal.lock().unwrap();
        let entry = journal
            .entries()
            .iter()
            .rev()
            .find(|e| !e.undone)
            .cloned()
            .ok_or_else(|| anyhow!("nothing to undo"))?;
        journal.undo(entry.id)?;
        Ok(entry)
    }

    pub fn clear_history(&self) {
        self.journal.lock().unwrap().clear();
    }
}

/// Files directly or recursively under `paths` (hidden entries skipped).
fn expand_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
    fn walk(path: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        if out.len() >= 5000 {
            return;
        }
        if path.is_file() {
            out.push(path.to_path_buf());
        } else if path.is_dir() && depth < 8 {
            let mut children: Vec<PathBuf> = std::fs::read_dir(path)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| !watcher::is_ignored(p))
                .collect();
            children.sort();
            for child in children {
                walk(&child, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    for p in paths {
        walk(p, 0, &mut out);
    }
    out
}

#[cfg(test)]
mod tests;

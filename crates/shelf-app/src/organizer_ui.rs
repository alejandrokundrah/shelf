//! The Organizer panel: Downloads watcher, batch image tool, rules editor and
//! undo history. All the logic lives in `shelf_core::organizer`; this file is
//! only the form-and-list UI around it.

use crate::{capsule, card, hairline, pal, row_toggle, semibold, shorten, toggle, Look};
use eframe::egui::{self, RichText};
use shelf_core::organizer::{
    human_size, next_rule_id, parse_rule, Action, Category, Condition, ImageFormat, JournalEntry,
    Organizer, OrganizerEvent, Plan, Rule,
};
use std::path::{Path, PathBuf};

const MB: f64 = 1_048_576.0;
const MAX_PLANS_SHOWN: usize = 40;

/// What happens to files dropped on the shelf.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DropMode {
    /// The normal Shelf behaviour: send to devices or stash.
    Send,
    /// Sort with the saved rules.
    Rules,
    /// Batch image tool: resize / convert / compress / rename.
    ImageTool,
}

/// The rule form. Plain text fields so the user never sees code.
#[derive(Clone, Default)]
struct RuleDraft {
    id: u64,
    name: String,
    category: Option<Category>,
    ext: String,
    contains: String,
    over_mb: String,
    under_mb: String,
    move_to: String,
    rename: String,
    resize_w: String,
    resize_h: String,
    convert: Option<ImageFormat>,
    compress: bool,
    quality: u8,
}

fn mb_text(bytes: u64) -> String {
    let v = bytes as f64 / MB;
    if (v - v.round()).abs() < 0.001 {
        format!("{}", v.round() as u64)
    } else {
        format!("{v:.1}")
    }
}

fn parse_mb(text: &str, what: &str) -> Result<Option<u64>, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    match t.parse::<f64>() {
        Ok(v) if v > 0.0 => Ok(Some((v * MB) as u64)),
        _ => Err(format!("\"{t}\" is not a valid size for {what} (use a number of MB)")),
    }
}

fn parse_px(text: &str, what: &str) -> Result<u32, String> {
    let t = text.trim().trim_end_matches("px").trim();
    if t.is_empty() {
        return Ok(0);
    }
    t.parse::<u32>().map_err(|_| format!("\"{t}\" is not a valid {what} (use whole pixels)"))
}

impl RuleDraft {
    fn blank() -> Self {
        RuleDraft { quality: 80, ..Default::default() }
    }

    fn from_rule(rule: &Rule) -> Self {
        let mut d = RuleDraft { id: rule.id, name: rule.name.clone(), quality: 80, ..Default::default() };
        for c in &rule.conditions {
            match c {
                Condition::Category(c) => d.category = Some(*c),
                Condition::Extension(e) => d.ext = e.clone(),
                Condition::NameContains(s) => d.contains = s.clone(),
                Condition::SizeOver(n) => d.over_mb = mb_text(*n),
                Condition::SizeUnder(n) => d.under_mb = mb_text(*n),
            }
        }
        for a in &rule.actions {
            match a {
                Action::MoveTo(p) => d.move_to = p.to_string_lossy().replace('\\', "/"),
                Action::Rename(t) => d.rename = t.clone(),
                Action::Resize { max_width, max_height } => {
                    d.resize_w = if *max_width > 0 { max_width.to_string() } else { String::new() };
                    d.resize_h = if *max_height > 0 { max_height.to_string() } else { String::new() };
                }
                Action::Convert(f) => d.convert = Some(*f),
                Action::Compress { quality } => {
                    d.compress = true;
                    d.quality = *quality;
                }
            }
        }
        d
    }

    fn to_rule(&self) -> Result<Rule, String> {
        let mut conditions = Vec::new();
        if let Some(c) = self.category {
            conditions.push(Condition::Category(c));
        }
        let ext = self.ext.trim().trim_start_matches('.').to_lowercase();
        if !ext.is_empty() {
            conditions.push(Condition::Extension(ext));
        }
        let contains = self.contains.trim();
        if !contains.is_empty() {
            conditions.push(Condition::NameContains(contains.to_string()));
        }
        if let Some(n) = parse_mb(&self.over_mb, "\"larger than\"")? {
            conditions.push(Condition::SizeOver(n));
        }
        if let Some(n) = parse_mb(&self.under_mb, "\"smaller than\"")? {
            conditions.push(Condition::SizeUnder(n));
        }
        if conditions.is_empty() {
            return Err("Pick at least one condition (a file type, an extension, a word in the name or a size).".into());
        }

        let mut actions = Vec::new();
        let (w, h) = (parse_px(&self.resize_w, "width")?, parse_px(&self.resize_h, "height")?);
        if w > 0 || h > 0 {
            actions.push(Action::Resize { max_width: w, max_height: h });
        }
        if let Some(f) = self.convert {
            actions.push(Action::Convert(f));
        }
        if self.compress {
            actions.push(Action::Compress { quality: self.quality.clamp(1, 100) });
        }
        if !self.rename.trim().is_empty() {
            actions.push(Action::Rename(self.rename.trim().to_string()));
        }
        let folder = self.move_to.trim().replace('\\', "/");
        if !folder.is_empty() {
            let mut path = PathBuf::new();
            for seg in folder.split('/').filter(|s| !s.trim().is_empty() && *s != "." && *s != "..") {
                path.push(seg.trim());
            }
            if path.as_os_str().is_empty() {
                return Err("That folder name is not usable.".into());
            }
            // An absolute path (C:\Work) keeps working: the leading segment survives.
            let path = if folder.starts_with('/') || folder.chars().nth(1) == Some(':') {
                PathBuf::from(&folder)
            } else {
                path
            };
            actions.push(Action::MoveTo(path));
        }
        if actions.is_empty() {
            return Err("Pick at least one action (move, rename, resize, convert or compress).".into());
        }
        let name = if self.name.trim().is_empty() { "My rule".to_string() } else { self.name.trim().to_string() };
        Ok(Rule { id: self.id, name, enabled: true, conditions, actions })
    }
}

enum Act {
    PickWatchFolder,
    TidyNow,
    AddFiles,
    ApplyOne(usize),
    SkipOne(usize),
    ApplyAll,
    ClearPlans,
    NewRule,
    EditRule(usize),
    DeleteRule(usize),
    MoveRuleUp(usize),
    SaveDraft,
    CancelDraft,
    MakeFromText,
    UndoLast,
    Undo(u64),
    ClearHistory,
}

pub struct OrganizerPanel {
    org: Organizer,
    pub mode: DropMode,
    plans: Vec<Plan>,
    note: Option<String>,

    // Batch image tool settings.
    img_format: Option<ImageFormat>,
    img_w: String,
    img_h: String,
    img_compress: bool,
    img_quality: u8,
    img_rename: String,
    img_folder: String,

    // Rules editor.
    draft: Option<RuleDraft>,
    draft_error: Option<String>,
    text_rule: String,
    text_note: Option<String>,
    text_error: Option<String>,
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

fn parent_name(p: &Path) -> String {
    p.parent().map(|d| d.display().to_string()).unwrap_or_default()
}

fn field(ui: &mut egui::Ui, label: &str, text: &mut String, hint: &str) {
    ui.label(RichText::new(label).size(11.5).color(pal().text2));
    ui.add(egui::TextEdit::singleline(text).hint_text(hint).desired_width(f32::INFINITY));
}

impl OrganizerPanel {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        let dir = shelf_core::data_dir().join("organizer");
        OrganizerPanel {
            org: Organizer::open(dir, wake),
            mode: DropMode::Send,
            plans: Vec::new(),
            note: None,
            img_format: Some(ImageFormat::Webp),
            img_w: String::new(),
            img_h: String::new(),
            img_compress: true,
            img_quality: 80,
            img_rename: String::new(),
            img_folder: "Pictures/Shelf Images".to_string(),
            draft: None,
            draft_error: None,
            text_rule: String::new(),
            text_note: None,
            text_error: None,
        }
    }

    /// Collects what the folder watcher did. Returns a line for the status bar
    /// when something changed.
    pub fn poll(&mut self) -> Option<String> {
        let mut message = None;
        for event in self.org.poll() {
            match event {
                OrganizerEvent::Proposed(plan) => {
                    if !self.plans.iter().any(|p| p.source == plan.source) {
                        message = Some(format!("Organizer: ready to sort {} (preview)", file_name(&plan.source)));
                        self.plans.push(plan);
                    }
                }
                OrganizerEvent::Applied(entry) => {
                    message = Some(format!(
                        "Organizer: moved {} to {}",
                        file_name(&entry.source),
                        shorten(&parent_name(&entry.dest), 36)
                    ));
                }
                OrganizerEvent::Failed { path, error } => {
                    message = Some(format!("Organizer: could not sort {}: {error}", file_name(&path)));
                }
            }
        }
        message
    }

    /// Files dropped on the shelf while the mode is not "Send".
    pub fn add_dropped(&mut self, paths: Vec<PathBuf>) -> String {
        let cfg = self.org.config();
        let base = cfg
            .watch_folder
            .clone()
            .or_else(|| paths.first().and_then(|p| p.parent().map(Path::to_path_buf)))
            .unwrap_or_default();
        let new_plans = match self.mode {
            DropMode::ImageTool => {
                let rule = self.image_tool_rule();
                self.org.plan_paths(&paths, Some(&[rule]), &base)
            }
            _ => self.org.plan_paths(&paths, None, &base),
        };
        let skipped = paths.len().saturating_sub(new_plans.len());
        let added = new_plans.len();
        for plan in new_plans {
            if !self.plans.iter().any(|p| p.source == plan.source) {
                self.plans.push(plan);
            }
        }
        if added == 0 {
            "Nothing to do for those files with the current settings".to_string()
        } else if skipped > 0 {
            format!("{added} file(s) ready to preview, {skipped} skipped (no matching rule or not images)")
        } else {
            format!("{added} file(s) ready. Check the preview, then apply.")
        }
    }

    fn image_tool_rule(&self) -> Rule {
        let mut actions = Vec::new();
        let w = parse_px(&self.img_w, "width").unwrap_or(0);
        let h = parse_px(&self.img_h, "height").unwrap_or(0);
        if w > 0 || h > 0 {
            actions.push(Action::Resize { max_width: w, max_height: h });
        }
        if let Some(f) = self.img_format {
            actions.push(Action::Convert(f));
        }
        if self.img_compress {
            actions.push(Action::Compress { quality: self.img_quality });
        }
        if !self.img_rename.trim().is_empty() {
            actions.push(Action::Rename(self.img_rename.trim().to_string()));
        }
        let folder = self.img_folder.trim();
        if !folder.is_empty() {
            actions.push(Action::MoveTo(PathBuf::from(folder.replace('\\', "/"))));
        }
        Rule {
            id: 0,
            name: "Image tool".into(),
            enabled: true,
            conditions: vec![Condition::Category(Category::Images)],
            actions,
        }
    }

    fn apply_plans(&mut self, indices: Vec<usize>) -> String {
        let chosen: Vec<Plan> = indices.iter().filter_map(|i| self.plans.get(*i).cloned()).collect();
        let (done, failed) = self.org.apply_all(&chosen);
        let done_sources: Vec<&PathBuf> = done.iter().map(|d| &d.source).collect();
        self.plans.retain(|p| !done_sources.contains(&&p.source));
        match (done.len(), failed.len()) {
            (0, 0) => "Nothing to do".into(),
            (n, 0) => format!("Done: {n} file(s). Use Undo if that wasn't right."),
            (n, f) => format!(
                "{n} done, {f} failed ({}). The failed files were left where they were.",
                shorten(&failed[0].1, 60)
            ),
        }
    }

    fn save_draft(&mut self, cfg: &mut shelf_core::organizer::Config) -> bool {
        let Some(draft) = &self.draft else { return false };
        match draft.to_rule() {
            Ok(mut rule) => {
                if rule.id == 0 {
                    rule.id = next_rule_id(&cfg.rules);
                    // The first matching rule wins, so a new rule goes on top.
                    cfg.rules.insert(0, rule);
                } else if let Some(slot) = cfg.rules.iter_mut().find(|r| r.id == rule.id) {
                    rule.enabled = slot.enabled;
                    *slot = rule;
                } else {
                    cfg.rules.insert(0, rule);
                }
                self.draft = None;
                self.draft_error = None;
                true
            }
            Err(e) => {
                self.draft_error = Some(e);
                false
            }
        }
    }

    /// Draws the panel. Returns a status line when something was done.
    pub fn show(&mut self, ui: &mut egui::Ui) -> Option<String> {
        let p = pal();
        let mut acts: Vec<Act> = Vec::new();
        let mut cfg = self.org.config();
        let before = (cfg.watching, cfg.auto_apply);
        let mut rules_changed = false;
        let history: Vec<JournalEntry> = self.org.history();
        let can_undo = history.iter().any(|e| !e.undone);

        // ---- Watch Downloads ------------------------------------------------
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(semibold("Watch a folder", 13.5).color(p.text));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if capsule(ui, "Undo last", Look::Glass, 26.0, None, can_undo).clicked() {
                        acts.push(Act::UndoLast);
                    }
                });
            });
            let folder = cfg
                .watch_folder
                .as_ref()
                .map(|f| f.display().to_string())
                .unwrap_or_else(|| "No folder chosen".to_string());
            ui.label(RichText::new(shorten(&folder, 44)).size(12.0).color(p.text2));
            ui.horizontal(|ui| {
                if capsule(ui, "Choose folder", Look::Glass, 26.0, None, true).clicked() {
                    acts.push(Act::PickWatchFolder);
                }
                if capsule(ui, "Tidy it now", Look::Glass, 26.0, None, cfg.watch_folder.is_some()).clicked() {
                    acts.push(Act::TidyNow);
                }
            });
            row_toggle(ui, "Watch for new files", "Sorts files after they finish downloading", &mut cfg.watching);
            let mut ask_first = !cfg.auto_apply;
            row_toggle(ui, "Ask me first", "Show what I'd do and wait for your OK", &mut ask_first);
            cfg.auto_apply = !ask_first;
            if cfg.auto_apply && cfg.watching {
                ui.label(
                    RichText::new("New files are moved automatically. Everything can be undone below.")
                        .size(11.5)
                        .color(p.text2),
                );
            }
        });

        // ---- Drop files / batch image tool ---------------------------------
        card(ui, |ui| {
            ui.label(semibold("When I drop files on the shelf", 13.5).color(p.text));
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
                for (mode, label) in [
                    (DropMode::Send, "Send them"),
                    (DropMode::Rules, "Sort with rules"),
                    (DropMode::ImageTool, "Image tool"),
                ] {
                    if capsule(ui, label, Look::Chip(self.mode == mode), 28.0, None, true).clicked() {
                        self.mode = mode;
                    }
                }
            });
            match self.mode {
                DropMode::Send => {
                    ui.label(RichText::new("Drops are sent to your devices or kept in the stash, as usual.").size(11.5).color(p.text2));
                }
                DropMode::Rules => {
                    ui.label(RichText::new("Drop files or folders: each one is matched against your rules, and you see a preview first.").size(11.5).color(p.text2));
                }
                DropMode::ImageTool => {
                    ui.label(RichText::new("Drop up to hundreds of images. Choose what to do to them, check the preview, then apply once.").size(11.5).color(p.text2));
                    ui.label(RichText::new("Convert to").size(11.5).color(p.text2));
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
                        for (fmt, label) in [
                            (None, "Keep"),
                            (Some(ImageFormat::Png), "PNG"),
                            (Some(ImageFormat::Jpeg), "JPG"),
                            (Some(ImageFormat::Webp), "WebP"),
                        ] {
                            if capsule(ui, label, Look::Chip(self.img_format == fmt), 26.0, None, true).clicked() {
                                self.img_format = fmt;
                            }
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.set_width((ui.available_width() / 2.0 - 6.0).max(60.0));
                            field(ui, "Max width (px)", &mut self.img_w, "any");
                        });
                        ui.vertical(|ui| {
                            ui.set_width(ui.available_width());
                            field(ui, "Max height (px)", &mut self.img_h, "any");
                        });
                    });
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Compress").size(13.0).color(p.text));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            toggle(ui, &mut self.img_compress);
                        });
                    });
                    if self.img_compress {
                        ui.add(egui::Slider::new(&mut self.img_quality, 10..=100).text("quality"));
                        if self.img_format != Some(ImageFormat::Jpeg)
                            && !(self.img_format.is_none())
                        {
                            ui.label(RichText::new("PNG and WebP are lossless here, so this only picks the strongest compression.").size(11.0).color(p.text2));
                        }
                    }
                    field(ui, "Rename to (optional)", &mut self.img_rename, "{date}_{name}   or   photo_{n}");
                    field(ui, "Save to folder", &mut self.img_folder, "Pictures/Shelf Images");
                    ui.label(RichText::new("Your originals are kept until you clear the history, so Undo always works.").size(11.0).color(p.text2));
                }
            }
            if self.mode != DropMode::Send
                && capsule(ui, "Add files…", Look::Primary, 28.0, None, true).clicked()
            {
                acts.push(Act::AddFiles);
            }
        });

        // ---- Preview: "here's what I'd do" ---------------------------------
        if !self.plans.is_empty() {
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(semibold(&format!("Here's what I'd do ({})", self.plans.len()), 13.5).color(p.text));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if capsule(ui, "Clear", Look::Tint(p.red), 26.0, None, true).clicked() {
                            acts.push(Act::ClearPlans);
                        }
                        if capsule(ui, "Apply all", Look::Primary, 26.0, None, true).clicked() {
                            acts.push(Act::ApplyAll);
                        }
                    });
                });
                for (i, plan) in self.plans.iter().take(MAX_PLANS_SHOWN).enumerate() {
                    hairline(ui);
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!(
                                "{}  →  {}",
                                shorten(&file_name(&plan.source), 26),
                                shorten(&file_name(&plan.dest), 26)
                            ))
                            .size(12.5)
                            .color(p.text),
                        )
                        .wrap(),
                    );
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!(
                                "{} · {} · rule: {}",
                                shorten(&parent_name(&plan.dest), 34),
                                human_size(plan.source_size),
                                shorten(&plan.rule, 24)
                            ))
                            .size(11.0)
                            .color(p.text2),
                        )
                        .wrap(),
                    );
                    if !plan.summary.is_empty() {
                        ui.add(
                            egui::Label::new(RichText::new(plan.summary.join(" · ")).size(11.0).color(p.text2))
                                .wrap(),
                        );
                    }
                    ui.horizontal(|ui| {
                        if capsule(ui, "Apply", Look::Glass, 24.0, None, true).clicked() {
                            acts.push(Act::ApplyOne(i));
                        }
                        if capsule(ui, "Skip", Look::Glass, 24.0, None, true).clicked() {
                            acts.push(Act::SkipOne(i));
                        }
                    });
                }
                if self.plans.len() > MAX_PLANS_SHOWN {
                    ui.label(
                        RichText::new(format!("…and {} more (Apply all handles them too)", self.plans.len() - MAX_PLANS_SHOWN))
                            .size(11.5)
                            .color(p.text2),
                    );
                }
            });
        }

        // ---- Rules ------------------------------------------------------------
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(semibold("Rules", 13.5).color(p.text));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if capsule(ui, "New rule", Look::Glass, 26.0, None, true).clicked() {
                        acts.push(Act::NewRule);
                    }
                });
            });
            ui.label(
                RichText::new("The first rule that matches a file is used. Rules near the top win.")
                    .size(11.0)
                    .color(p.text2),
            );

            // Plain English → rule.
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.text_rule)
                        .hint_text("Type a rule: put school PDFs in Study")
                        .desired_width((ui.available_width() - 84.0).max(60.0)),
                );
                if capsule(ui, "Make rule", Look::Primary, 28.0, Some(76.0), !self.text_rule.trim().is_empty()).clicked() {
                    acts.push(Act::MakeFromText);
                }
            });
            if let Some(note) = &self.text_note {
                ui.add(egui::Label::new(RichText::new(note).size(11.5).color(p.accent)).wrap());
            }
            if let Some(err) = &self.text_error {
                ui.add(egui::Label::new(RichText::new(err).size(11.5).color(p.red)).wrap());
            }

            let count = cfg.rules.len();
            for (i, rule) in cfg.rules.iter_mut().enumerate() {
                hairline(ui);
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width((ui.available_width() - 52.0).max(80.0));
                        ui.add(egui::Label::new(semibold(&rule.name, 12.5).color(p.text)).wrap());
                        ui.add(egui::Label::new(RichText::new(rule.describe()).size(11.0).color(p.text2)).wrap());
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if toggle(ui, &mut rule.enabled).clicked() {
                            rules_changed = true;
                        }
                    });
                });
                ui.horizontal(|ui| {
                    if capsule(ui, "Edit", Look::Glass, 22.0, None, true).clicked() {
                        acts.push(Act::EditRule(i));
                    }
                    if i > 0 && capsule(ui, "Up", Look::Glass, 22.0, None, true).clicked() {
                        acts.push(Act::MoveRuleUp(i));
                    }
                    if capsule(ui, "Delete", Look::Tint(p.red), 22.0, None, count > 0).clicked() {
                        acts.push(Act::DeleteRule(i));
                    }
                });
            }
        });

        // ---- Rule form ----------------------------------------------------------
        if let Some(d) = &mut self.draft {
            card(ui, |ui| {
                ui.label(semibold(if d.id == 0 { "New rule" } else { "Edit rule" }, 13.5).color(p.text));
                field(ui, "Name", &mut d.name, "My rule");
                hairline(ui);
                ui.label(semibold("When the file…", 12.5).color(p.text));
                ui.label(RichText::new("Type").size(11.5).color(p.text2));
                egui::ComboBox::from_id_salt("org-cat")
                    .selected_text(d.category.map(|c| c.label()).unwrap_or("Any type"))
                    .width(160.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut d.category, None, "Any type");
                        for c in Category::ALL {
                            ui.selectable_value(&mut d.category, Some(c), c.label());
                        }
                    });
                field(ui, "Extension is", &mut d.ext, "e.g. png");
                field(ui, "Name contains", &mut d.contains, "e.g. invoice");
                field(ui, "Larger than (MB)", &mut d.over_mb, "e.g. 2");
                field(ui, "Smaller than (MB)", &mut d.under_mb, "e.g. 50");
                hairline(ui);
                ui.label(semibold("Then…", 12.5).color(p.text));
                field(ui, "Move to folder", &mut d.move_to, "Study   or   Pictures/Designs");
                field(ui, "Rename to", &mut d.rename, "{date}_{name}");
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width((ui.available_width() / 2.0 - 6.0).max(60.0));
                        field(ui, "Resize: max width", &mut d.resize_w, "px");
                    });
                    ui.vertical(|ui| {
                        ui.set_width(ui.available_width());
                        field(ui, "max height", &mut d.resize_h, "px");
                    });
                });
                ui.label(RichText::new("Convert to").size(11.5).color(p.text2));
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
                    for (fmt, label) in [
                        (None, "Keep"),
                        (Some(ImageFormat::Png), "PNG"),
                        (Some(ImageFormat::Jpeg), "JPG"),
                        (Some(ImageFormat::Webp), "WebP"),
                    ] {
                        if capsule(ui, label, Look::Chip(d.convert == fmt), 24.0, None, true).clicked() {
                            d.convert = fmt;
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Compress").size(13.0).color(p.text));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        toggle(ui, &mut d.compress);
                    });
                });
                if d.compress {
                    ui.add(egui::Slider::new(&mut d.quality, 10..=100).text("quality"));
                }
                ui.label(
                    RichText::new("Tokens for Rename: {name} {date} {n} {ext}")
                        .size(11.0)
                        .color(p.text2),
                );
                if let Some(err) = &self.draft_error {
                    ui.add(egui::Label::new(RichText::new(err).size(11.5).color(p.red)).wrap());
                }
                ui.horizontal(|ui| {
                    if capsule(ui, "Save rule", Look::Primary, 28.0, None, true).clicked() {
                        acts.push(Act::SaveDraft);
                    }
                    if capsule(ui, "Cancel", Look::Glass, 28.0, None, true).clicked() {
                        acts.push(Act::CancelDraft);
                    }
                });
            });
        }

        // ---- Undo history ---------------------------------------------------------
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(semibold("Undo history", 13.5).color(p.text));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if !history.is_empty()
                        && capsule(ui, "Clear history", Look::Tint(p.red), 26.0, None, true).clicked()
                    {
                        acts.push(Act::ClearHistory);
                    }
                });
            });
            if history.is_empty() {
                ui.label(RichText::new("Nothing has been sorted yet. Every move shows up here with an Undo button.").size(11.5).color(p.text2));
            }
            for entry in history.iter().take(20) {
                hairline(ui);
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width((ui.available_width() - 64.0).max(80.0));
                        let color = if entry.undone { p.text2 } else { p.text };
                        ui.add(
                            egui::Label::new(
                                RichText::new(format!(
                                    "{} → {}",
                                    shorten(&file_name(&entry.source), 22),
                                    shorten(&file_name(&entry.dest), 22)
                                ))
                                .size(12.0)
                                .color(color),
                            )
                            .wrap(),
                        );
                        ui.add(
                            egui::Label::new(
                                RichText::new(format!(
                                    "{} · {} · {}",
                                    entry.when(),
                                    shorten(&parent_name(&entry.dest), 24),
                                    shorten(&entry.rule, 20)
                                ))
                                .size(10.5)
                                .color(p.text2),
                            )
                            .wrap(),
                        );
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if entry.undone {
                            ui.label(RichText::new("Undone").size(11.5).color(p.text2));
                        } else if capsule(ui, "Undo", Look::Glass, 24.0, None, true).clicked() {
                            acts.push(Act::Undo(entry.id));
                        }
                    });
                });
            }
        });

        // ---- Carry out what was clicked ------------------------------------------
        let mut status: Option<String> = None;
        let mut save_cfg = rules_changed || before != (cfg.watching, cfg.auto_apply);
        for act in acts {
            match act {
                Act::PickWatchFolder => {
                    if let Some(dir) = rfd::FileDialog::new().set_title("Folder to keep tidy").pick_folder() {
                        cfg.watch_folder = Some(dir);
                        save_cfg = true;
                    }
                }
                Act::TidyNow => {
                    // Saved first so "tidy" uses the folder and rules shown.
                    self.org.update_config(|c| *c = cfg.clone());
                    save_cfg = false;
                    let found = self.org.plan_existing();
                    let added = found.len();
                    for plan in found {
                        if !self.plans.iter().any(|p| p.source == plan.source) {
                            self.plans.push(plan);
                        }
                    }
                    status = Some(if added == 0 {
                        "Nothing in that folder matches a rule".to_string()
                    } else {
                        format!("{added} file(s) ready to preview")
                    });
                }
                Act::AddFiles => {
                    let picked = match self.mode {
                        DropMode::ImageTool => rfd::FileDialog::new()
                            .set_title("Choose images")
                            .add_filter("Images", &["png", "jpg", "jpeg", "webp", "gif", "bmp"])
                            .pick_files(),
                        _ => rfd::FileDialog::new().set_title("Choose files").pick_files(),
                    };
                    if let Some(paths) = picked {
                        status = Some(self.add_dropped(paths));
                    }
                }
                Act::ApplyOne(i) => status = Some(self.apply_plans(vec![i])),
                Act::SkipOne(i) => {
                    if i < self.plans.len() {
                        self.plans.remove(i);
                    }
                }
                Act::ApplyAll => {
                    let all: Vec<usize> = (0..self.plans.len()).collect();
                    status = Some(self.apply_plans(all));
                }
                Act::ClearPlans => self.plans.clear(),
                Act::NewRule => {
                    self.draft = Some(RuleDraft::blank());
                    self.draft_error = None;
                }
                Act::EditRule(i) => {
                    if let Some(rule) = cfg.rules.get(i) {
                        self.draft = Some(RuleDraft::from_rule(rule));
                        self.draft_error = None;
                    }
                }
                Act::DeleteRule(i) => {
                    if i < cfg.rules.len() {
                        cfg.rules.remove(i);
                        save_cfg = true;
                    }
                }
                Act::MoveRuleUp(i) => {
                    if i > 0 && i < cfg.rules.len() {
                        cfg.rules.swap(i, i - 1);
                        save_cfg = true;
                    }
                }
                Act::SaveDraft => {
                    if self.save_draft(&mut cfg) {
                        save_cfg = true;
                        self.text_note = None;
                        self.text_rule.clear();
                        status = Some("Rule saved".to_string());
                    }
                }
                Act::CancelDraft => {
                    self.draft = None;
                    self.draft_error = None;
                }
                Act::MakeFromText => match parse_rule(&self.text_rule) {
                    Ok(rule) => {
                        self.text_note = Some(format!("I understood: {}. Check it below, then Save rule.", rule.describe()));
                        self.text_error = None;
                        self.draft = Some(RuleDraft::from_rule(&rule));
                        self.draft_error = None;
                    }
                    Err(e) => {
                        self.text_error = Some(e);
                        self.text_note = None;
                    }
                },
                Act::UndoLast => {
                    status = Some(match self.org.undo_last() {
                        Ok(e) => format!("Undone: {} is back where it was", file_name(&e.source)),
                        Err(e) => format!("Couldn't undo: {e}"),
                    });
                }
                Act::Undo(id) => {
                    status = Some(match self.org.undo(id) {
                        Ok(()) => "Undone".to_string(),
                        Err(e) => format!("Couldn't undo: {e}"),
                    });
                }
                Act::ClearHistory => {
                    self.org.clear_history();
                    status = Some("History cleared. Earlier moves can no longer be undone.".to_string());
                }
            }
        }
        if save_cfg {
            self.org.update_config(|c| *c = cfg);
        }
        if status.is_some() {
            self.note = status.clone();
        }
        status
    }
}

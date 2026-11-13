#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod clip;

use eframe::egui::{self, Align, Color32, Layout, RichText};
use shelf_core::{
    Command, Event, Handle, ItemKind, NearbyDevice, Payload, PeerStatus, ReceivedItem, Target,
};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

const COLLAPSED_W: f32 = 14.0;
const EXPANDED_W: f32 = 360.0;
const ITEM_TTL: Duration = Duration::from_secs(3600);
const BG: Color32 = Color32::from_rgb(24, 26, 33);
const PANEL: Color32 = Color32::from_rgb(34, 37, 47);
const ACCENT: Color32 = Color32::from_rgb(108, 140, 255);
const GOOD: Color32 = Color32::from_rgb(110, 200, 140);

fn load_icon() -> Option<egui::IconData> {
    let bytes = include_bytes!("../assets/icon.png");
    let decoder = png::Decoder::new(std::io::Cursor::new(&bytes[..]));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    Some(egui::IconData {
        rgba: buf[..info.buffer_size()].to_vec(),
        width: info.width,
        height: info.height,
    })
}

fn main() -> eframe::Result<()> {
    // Only one Shelf per PC: starting it a second time just exits quietly.
    let _single_instance = match std::net::TcpListener::bind("127.0.0.1:47831") {
        Ok(l) => l,
        Err(_) => return Ok(()),
    };

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Shelf")
        .with_inner_size([COLLAPSED_W, 560.0])
        .with_position([1900.0, 200.0])
        .with_decorations(false)
        .with_resizable(false)
        .with_always_on_top();
    if let Some(icon) = load_icon() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "Shelf",
        options,
        Box::new(|cc| Ok(Box::new(ShelfApp::new(cc)))),
    )
}

struct ShelfItem {
    item: ReceivedItem,
    received: Instant,
}

struct PairDialog {
    device: NearbyDevice,
    code: String,
    busy: bool,
    error: Option<String>,
}

enum Action {
    SendClipboard,
    StartPairing,
    CancelPairing,
    OpenPair(NearbyDevice),
    ConnectPair,
    ClosePair,
    Unpair(String),
    Copy(usize),
    Reveal(usize),
    Remove(usize),
}

struct ShelfApp {
    ctx: egui::Context,
    handle: Handle,
    rx: Receiver<Event>,
    clip_tx: Sender<Result<Payload, String>>,
    clip_rx: Receiver<Result<Payload, String>>,

    device_name: String,
    paired: Vec<PeerStatus>,
    nearby: Vec<NearbyDevice>,
    items: Vec<ShelfItem>,
    target: Target,
    status: String,
    progress: Option<(String, u8)>,
    my_code: Option<(String, Instant)>,
    pair_dialog: Option<PairDialog>,

    auto_copy: bool,
    pinned: bool,

    cur_w: f32,
    applied_w: f32,
    positioned: bool,
    last_active: Instant,
    open_until: Instant,
}

impl ShelfApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = cc.egui_ctx.clone();
        let wake_ctx = ctx.clone();
        let (handle, rx) =
            shelf_core::start(move || wake_ctx.request_repaint()).expect("failed to start Shelf");
        let (clip_tx, clip_rx) = channel();
        sweep_inbox();
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = BG;
        ctx.set_visuals(visuals);
        let now = Instant::now();
        Self {
            ctx,
            handle,
            rx,
            clip_tx,
            clip_rx,
            device_name: String::new(),
            paired: Vec::new(),
            nearby: Vec::new(),
            items: Vec::new(),
            target: Target::All,
            status: String::new(),
            progress: None,
            my_code: None,
            pair_dialog: None,
            auto_copy: true,
            pinned: false,
            cur_w: COLLAPSED_W,
            applied_w: -1.0,
            positioned: false,
            last_active: now - Duration::from_secs(10),
            open_until: now,
        }
    }

    fn set_status(&mut self, s: impl Into<String>) {
        self.status = s.into();
    }

    fn open_for(&mut self, secs: u64) {
        self.open_until = Instant::now() + Duration::from_secs(secs);
    }

    fn drain_events(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Event::Ready { device_name, .. } => self.device_name = device_name,
                Event::Peers { paired, nearby } => {
                    self.paired = paired;
                    self.nearby = nearby;
                    if let Target::Peer(k) = &self.target {
                        if !self.paired.iter().any(|p| &p.public_key == k) {
                            self.target = Target::All;
                        }
                    }
                }
                Event::PairingCode { code, seconds } => {
                    self.my_code = Some((code, Instant::now() + Duration::from_secs(seconds)));
                }
                Event::PairingCleared => self.my_code = None,
                Event::PairingResult(res) => match res {
                    Ok(name) => {
                        self.pair_dialog = None;
                        self.my_code = None;
                        self.set_status(format!("Paired with {name}"));
                        self.open_for(4);
                    }
                    Err(e) => {
                        if let Some(d) = self.pair_dialog.as_mut() {
                            d.busy = false;
                            d.error = Some(e);
                        } else {
                            self.set_status(format!("Pairing failed: {e}"));
                        }
                    }
                },
                Event::Received(item) => {
                    if self.auto_copy {
                        clip::copy_item(&item);
                    }
                    self.set_status(format!("Received {} from {}", item.name, item.from));
                    self.progress = None;
                    self.items.insert(0, ShelfItem { item, received: Instant::now() });
                    self.open_for(4);
                }
                Event::Progress { name, to, percent } => {
                    self.progress = Some((format!("{name} to {to}"), percent));
                }
                Event::SendDone { to, item, error } => {
                    self.progress = None;
                    match error {
                        None => self.set_status(format!("Sent {item} to {to}")),
                        Some(e) => self.set_status(format!("Could not send {item} to {to}: {e}")),
                    }
                }
                Event::Info(s) => self.set_status(s),
                Event::Error(s) => self.set_status(s),
            }
        }
        while let Ok(res) = self.clip_rx.try_recv() {
            match res {
                Ok(payload) => {
                    self.set_status("Sending clipboard...");
                    self.handle.send(Command::Send { target: self.target.clone(), payload });
                }
                Err(e) => self.set_status(e),
            }
        }
    }

    fn send_clipboard(&mut self) {
        self.set_status("Reading clipboard...");
        let tx = self.clip_tx.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let res = clip::read_clipboard().map_err(|e| e.to_string());
            let _ = tx.send(res);
            ctx.request_repaint();
        });
    }

    fn apply_geometry(&mut self, ctx: &egui::Context, expanded: bool) {
        let Some(monitor) = ctx.input(|i| i.viewport().monitor_size) else {
            ctx.request_repaint_after(Duration::from_millis(100));
            return;
        };
        let target_w = if expanded { EXPANDED_W } else { COLLAPSED_W };
        let h = (monitor.y - 160.0).clamp(420.0, 680.0);
        if (self.cur_w - target_w).abs() < 0.5 {
            self.cur_w = target_w;
        } else {
            self.cur_w += (target_w - self.cur_w) * 0.35;
            ctx.request_repaint();
        }
        if !self.positioned || (self.cur_w - self.applied_w).abs() > 0.01 {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(self.cur_w, h)));
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(
                monitor.x - self.cur_w,
                (monitor.y - h) / 2.0,
            )));
            self.applied_w = self.cur_w;
            self.positioned = true;
        }
    }

    fn target_label(&self) -> String {
        match &self.target {
            Target::All => "All paired devices".to_string(),
            Target::Peer(k) => self
                .paired
                .iter()
                .find(|p| &p.public_key == k)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| "Unknown".to_string()),
        }
    }
}

impl Drop for ShelfApp {
    fn drop(&mut self) {
        // Lets the engine say goodbye on the network so peers drop us from their list.
        self.handle.send(Command::Shutdown);
        std::thread::sleep(Duration::from_millis(400));
    }
}

fn sweep_inbox() {
    if let Ok(entries) = std::fs::read_dir(shelf_core::inbox_dir()) {
        for e in entries.flatten() {
            let old = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .map(|age| age > ITEM_TTL)
                .unwrap_or(false);
            if old {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

fn human_size(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(3)).collect();
        format!("{cut}...")
    }
}

impl eframe::App for ShelfApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();

        // Files dropped onto the shelf are sent to the chosen device(s).
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect()
        });
        if !dropped.is_empty() {
            self.set_status(format!("Sending {} file(s)...", dropped.len()));
            self.handle.send(Command::Send {
                target: self.target.clone(),
                payload: Payload::Files(dropped),
            });
            self.open_for(3);
        }

        // Expire old items.
        self.items.retain(|it| {
            if it.received.elapsed() > ITEM_TTL {
                if let Some(p) = &it.item.path {
                    let _ = std::fs::remove_file(p);
                }
                false
            } else {
                true
            }
        });
        if let Some((_, until)) = &self.my_code {
            if Instant::now() > *until {
                self.my_code = None;
            }
        }

        let hovering_files = ctx.input(|i| !i.raw.hovered_files.is_empty());
        let pointer_inside = ctx.input(|i| i.pointer.hover_pos().is_some());
        if pointer_inside || hovering_files {
            self.last_active = Instant::now();
        }
        let expanded = self.pinned
            || hovering_files
            || self.last_active.elapsed() < Duration::from_millis(1200)
            || Instant::now() < self.open_until
            || self.my_code.is_some()
            || self.pair_dialog.is_some();
        self.apply_geometry(ctx, expanded);

        let collapsed = self.cur_w < 80.0;
        let paired = self.paired.clone();
        let nearby = self.nearby.clone();
        let mut actions: Vec<Action> = Vec::new();

        let frame = egui::Frame::none()
            .fill(if collapsed { ACCENT } else { BG })
            .inner_margin(egui::Margin::same(if collapsed { 0.0 } else { 12.0 }));

        egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
            if collapsed {
                return;
            }
            ui.set_min_width(EXPANDED_W - 24.0);

            ui.horizontal(|ui| {
                ui.heading("Shelf");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("Quit").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    ui.checkbox(&mut self.pinned, "Pin open");
                });
            });
            ui.label(RichText::new(format!("This PC: {}", self.device_name)).small().weak());
            if !self.status.is_empty() {
                ui.label(RichText::new(self.status.clone()).small().color(ACCENT));
            }
            if let Some((label, pct)) = &self.progress {
                ui.add(
                    egui::ProgressBar::new(*pct as f32 / 100.0)
                        .text(format!("{} ({}%)", shorten(label, 30), pct)),
                );
            }
            ui.separator();

            // Where things go.
            let label = self.target_label();
            egui::ComboBox::from_label("Send to")
                .selected_text(label)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.target, Target::All, "All paired devices");
                    for p in &paired {
                        ui.selectable_value(
                            &mut self.target,
                            Target::Peer(p.public_key.clone()),
                            p.name.clone(),
                        );
                    }
                });
            ui.add_space(6.0);

            // Drop zone.
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), 90.0),
                egui::Sense::hover(),
            );
            let color = if hovering_files { ACCENT } else { Color32::from_gray(100) };
            ui.painter().rect_stroke(rect, 8.0, egui::Stroke::new(2.0, color));
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                if hovering_files { "Release to send" } else { "Drop files here" },
                egui::FontId::proportional(16.0),
                color,
            );
            ui.add_space(6.0);

            ui.horizontal(|ui| {
                if ui.button("Send clipboard").clicked() {
                    actions.push(Action::SendClipboard);
                }
                ui.checkbox(&mut self.auto_copy, "Auto-copy received");
            });
            ui.add_space(4.0);

            // Devices and pairing.
            egui::CollapsingHeader::new("Devices")
                .default_open(paired.is_empty() || self.my_code.is_some() || self.pair_dialog.is_some())
                .show(ui, |ui| {
                    for p in &paired {
                        ui.horizontal(|ui| {
                            ui.label(shorten(&p.name, 24));
                            if p.addr.is_some() {
                                ui.label(RichText::new("online").small().color(GOOD));
                            } else {
                                ui.label(RichText::new("offline").small().weak());
                            }
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.small_button("Remove").clicked() {
                                    actions.push(Action::Unpair(p.public_key.clone()));
                                }
                            });
                        });
                    }
                    if !nearby.is_empty() {
                        ui.add_space(4.0);
                        ui.label(RichText::new("Nearby, not paired").small().weak());
                    }
                    for d in &nearby {
                        ui.horizontal(|ui| {
                            ui.label(shorten(&d.name, 24));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.small_button("Pair").clicked() {
                                    actions.push(Action::OpenPair(d.clone()));
                                }
                            });
                        });
                    }
                    ui.add_space(4.0);

                    if let Some((code, until)) = &self.my_code {
                        let left = until.saturating_duration_since(Instant::now()).as_secs();
                        let spaced = format!("{} {}", &code[..3], &code[3..]);
                        ui.group(|ui| {
                            ui.label("Enter this code on the other PC:");
                            ui.label(RichText::new(spaced).monospace().size(28.0).strong());
                            ui.label(RichText::new(format!("Expires in {left}s")).small().weak());
                            if ui.small_button("Cancel").clicked() {
                                actions.push(Action::CancelPairing);
                            }
                        });
                    } else if self.pair_dialog.is_none() {
                        if ui.button("Show a pairing code on this PC").clicked() {
                            actions.push(Action::StartPairing);
                        }
                    }

                    if let Some(d) = self.pair_dialog.as_mut() {
                        ui.group(|ui| {
                            ui.label(format!("Pair with {}", shorten(&d.device.name, 24)));
                            ui.label(
                                RichText::new("Type the 6 digit code shown on that PC.")
                                    .small()
                                    .weak(),
                            );
                            ui.add(
                                egui::TextEdit::singleline(&mut d.code)
                                    .desired_width(120.0)
                                    .char_limit(6)
                                    .hint_text("123456"),
                            );
                            if let Some(e) = &d.error {
                                ui.label(RichText::new(e.clone()).small().color(Color32::LIGHT_RED));
                            }
                            ui.horizontal(|ui| {
                                let valid = d.code.len() == 6 && d.code.chars().all(|c| c.is_ascii_digit());
                                if ui.add_enabled(valid && !d.busy, egui::Button::new("Connect")).clicked() {
                                    actions.push(Action::ConnectPair);
                                }
                                if ui.button("Cancel").clicked() {
                                    actions.push(Action::ClosePair);
                                }
                                if d.busy {
                                    ui.spinner();
                                }
                            });
                        });
                    }
                });
            ui.separator();

            // The shelf itself.
            ui.label(RichText::new("On the shelf").strong());
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                if self.items.is_empty() {
                    ui.label(RichText::new("Nothing here yet.").weak());
                }
                for (i, it) in self.items.iter().enumerate() {
                    egui::Frame::none()
                        .fill(PANEL)
                        .rounding(6.0)
                        .inner_margin(egui::Margin::same(8.0))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            let kind = match it.item.kind {
                                ItemKind::File => "FILE",
                                ItemKind::Text => "TEXT",
                                ItemKind::Image => "IMAGE",
                            };
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(kind).small().color(ACCENT));
                                ui.label(RichText::new(shorten(&it.item.name, 30)).strong());
                            });
                            if let Some(t) = &it.item.text {
                                ui.label(RichText::new(shorten(&t.replace('\n', " "), 70)).small());
                            }
                            let mins = ITEM_TTL.saturating_sub(it.received.elapsed()).as_secs() / 60;
                            ui.label(
                                RichText::new(format!(
                                    "from {} - {} - {} min left",
                                    it.item.from,
                                    human_size(it.item.size),
                                    mins
                                ))
                                .small()
                                .weak(),
                            );
                            ui.horizontal(|ui| {
                                if ui.small_button("Copy").clicked() {
                                    actions.push(Action::Copy(i));
                                }
                                if it.item.path.is_some() && ui.small_button("Show").clicked() {
                                    actions.push(Action::Reveal(i));
                                }
                                if ui.small_button("Remove").clicked() {
                                    actions.push(Action::Remove(i));
                                }
                            });
                        });
                    ui.add_space(4.0);
                }
            });
        });

        // Apply what the user clicked.
        let mut remove: Vec<usize> = Vec::new();
        for a in actions {
            match a {
                Action::SendClipboard => self.send_clipboard(),
                Action::StartPairing => self.handle.send(Command::StartPairing),
                Action::CancelPairing => {
                    self.handle.send(Command::CancelPairing);
                    self.my_code = None;
                }
                Action::OpenPair(device) => {
                    self.my_code = None;
                    self.handle.send(Command::CancelPairing);
                    self.pair_dialog = Some(PairDialog {
                        device,
                        code: String::new(),
                        busy: false,
                        error: None,
                    });
                }
                Action::ConnectPair => {
                    if let Some(d) = self.pair_dialog.as_mut() {
                        d.busy = true;
                        d.error = None;
                        self.handle.send(Command::PairWith {
                            addr: d.device.addr,
                            code: d.code.clone(),
                        });
                    }
                }
                Action::ClosePair => self.pair_dialog = None,
                Action::Unpair(k) => self.handle.send(Command::Unpair { public_key: k }),
                Action::Copy(i) => {
                    if let Some(it) = self.items.get(i) {
                        clip::copy_item(&it.item);
                        self.status = "Copied to clipboard".to_string();
                    }
                }
                Action::Reveal(i) => {
                    if let Some(p) = self.items.get(i).and_then(|it| it.item.path.clone()) {
                        clip::reveal(&p);
                    }
                }
                Action::Remove(i) => remove.push(i),
            }
        }
        remove.sort_unstable();
        for i in remove.into_iter().rev() {
            if i < self.items.len() {
                let it = self.items.remove(i);
                if let Some(p) = it.item.path {
                    let _ = std::fs::remove_file(p);
                }
            }
        }

        // Keeps countdowns and expiry ticking even when nothing else happens.
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

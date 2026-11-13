#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod clip;

use eframe::egui::{self, vec2, Align, Color32, Layout, Pos2, Rect, RichText, Rounding, Stroke};
use shelf_core::{
    Command, Event, Handle, ItemKind, NearbyDevice, Payload, PeerStatus, ReceivedItem, Target,
};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

const COLLAPSED_W: f32 = 14.0;
const EXPANDED_W: f32 = 360.0;
const ITEM_TTL: Duration = Duration::from_secs(3600);

// macOS dark palette.
const BG: Color32 = Color32::from_rgb(30, 30, 32);
const CARD: Color32 = Color32::from_rgb(44, 44, 46);
const TEXT: Color32 = Color32::from_rgb(245, 245, 247);
const TEXT2: Color32 = Color32::from_rgb(152, 152, 157);
const TEXT3: Color32 = Color32::from_rgb(99, 99, 102);
const ACCENT: Color32 = Color32::from_rgb(10, 132, 255);
const GOOD: Color32 = Color32::from_rgb(48, 209, 88);
const RED: Color32 = Color32::from_rgb(255, 69, 58);
const ORANGE: Color32 = Color32::from_rgb(255, 159, 10);
const PURPLE: Color32 = Color32::from_rgb(191, 90, 242);
const SEP: Color32 = Color32::from_rgba_premultiplied(24, 24, 24, 24);

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
        .with_transparent(true)
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

// ---------------------------------------------------------------------------
// Theme and small widgets
// ---------------------------------------------------------------------------

fn apply_mac_style(ctx: &egui::Context) {
    // Use the system UI font on Windows when available (closest to San Francisco).
    let mut fonts = egui::FontDefinitions::default();
    if let Ok(bytes) = std::fs::read("C:\\Windows\\Fonts\\segoeui.ttf") {
        fonts
            .font_data
            .insert("system-ui".to_owned(), egui::FontData::from_owned(bytes).into());
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "system-ui".to_owned());
    }
    ctx.set_fonts(fonts);

    use egui::{FontId, TextStyle};
    let mut style = (*ctx.style()).clone();
    style.text_styles = [
        (TextStyle::Heading, FontId::proportional(17.0)),
        (TextStyle::Body, FontId::proportional(13.0)),
        (TextStyle::Button, FontId::proportional(13.0)),
        (TextStyle::Small, FontId::proportional(11.0)),
        (TextStyle::Monospace, FontId::monospace(13.0)),
    ]
    .into();
    style.spacing.item_spacing = vec2(8.0, 10.0);
    style.spacing.button_padding = vec2(12.0, 5.0);
    style.spacing.interact_size.y = 24.0;

    style.visuals = egui::Visuals::dark();
    let v = &mut style.visuals;
    let r = Rounding::same(6.0);
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.window_stroke = Stroke::new(1.0, SEP);
    v.window_rounding = Rounding::same(10.0);
    v.menu_rounding = Rounding::same(8.0);
    v.extreme_bg_color = Color32::from_rgb(24, 24, 26);
    v.faint_bg_color = CARD;
    v.weak_text_color = Some(TEXT2);
    v.hyperlink_color = ACCENT;
    v.selection.bg_fill = ACCENT;
    v.selection.stroke = Stroke::new(1.0, Color32::WHITE);

    v.widgets.noninteractive.bg_fill = CARD;
    v.widgets.noninteractive.weak_bg_fill = CARD;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, SEP);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.noninteractive.rounding = r;
    v.widgets.noninteractive.expansion = 0.0;

    v.widgets.inactive.bg_fill = Color32::from_white_alpha(24);
    v.widgets.inactive.weak_bg_fill = Color32::from_white_alpha(24);
    v.widgets.inactive.bg_stroke = Stroke::NONE;
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.inactive.rounding = r;
    v.widgets.inactive.expansion = 0.0;

    v.widgets.hovered.bg_fill = Color32::from_white_alpha(38);
    v.widgets.hovered.weak_bg_fill = Color32::from_white_alpha(38);
    v.widgets.hovered.bg_stroke = Stroke::NONE;
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.hovered.rounding = r;
    v.widgets.hovered.expansion = 0.0;

    v.widgets.active.bg_fill = Color32::from_white_alpha(56);
    v.widgets.active.weak_bg_fill = Color32::from_white_alpha(56);
    v.widgets.active.bg_stroke = Stroke::NONE;
    v.widgets.active.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.active.rounding = r;
    v.widgets.active.expansion = 0.0;

    v.widgets.open.bg_fill = Color32::from_white_alpha(38);
    v.widgets.open.weak_bg_fill = Color32::from_white_alpha(38);
    v.widgets.open.bg_stroke = Stroke::NONE;
    v.widgets.open.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.open.rounding = r;
    v.widgets.open.expansion = 0.0;

    ctx.set_style(style);
}

/// A rounded "grouped" container, like the inset lists in macOS System Settings.
fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::none()
        .fill(CARD)
        .rounding(10.0)
        .inner_margin(egui::Margin::symmetric(12.0, 10.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 6.0;
            add(ui)
        })
        .inner
}

fn hairline(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 0.0, SEP);
}

fn section_label(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text.to_uppercase()).size(11.0).color(TEXT2));
}

fn status_dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(8.0, 8.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

fn quit_dot(ui: &mut egui::Ui) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(13.0, 13.0), egui::Sense::click());
    let c = rect.center();
    ui.painter().circle_filled(c, 6.5, Color32::from_rgb(255, 95, 87));
    if resp.hovered() {
        let s = Stroke::new(1.3, Color32::from_rgb(90, 20, 15));
        ui.painter().line_segment([c + vec2(-2.5, -2.5), c + vec2(2.5, 2.5)], s);
        ui.painter().line_segment([c + vec2(2.5, -2.5), c + vec2(-2.5, 2.5)], s);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("Quit Shelf")
}

/// macOS-style switch.
fn toggle(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let (rect, mut resp) = ui.allocate_exact_size(vec2(38.0, 22.0), egui::Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_responsive(resp.id, *on);
    let track = Color32::from_rgb(88, 88, 92).lerp_to_gamma(ACCENT, t);
    let x = egui::lerp((rect.left() + 11.0)..=(rect.right() - 11.0), t);
    let p = ui.painter();
    p.rect_filled(rect, 11.0, track);
    p.circle_filled(Pos2::new(x, rect.center().y + 0.5), 9.0, Color32::from_black_alpha(60));
    p.circle_filled(Pos2::new(x, rect.center().y), 9.0, Color32::WHITE);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn row_toggle(ui: &mut egui::Ui, label: &str, value: &mut bool) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).color(TEXT));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            toggle(ui, value);
        });
    });
}

fn primary_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_owned()).size(13.0).color(Color32::WHITE))
        .fill(ACCENT)
        .rounding(7.0)
}

fn pill_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_owned()).size(12.0).color(Color32::WHITE))
        .fill(ACCENT)
        .rounding(11.0)
}

fn link_button(ui: &mut egui::Ui, text: &str, color: Color32) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(text).size(12.0).color(color)).frame(false))
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn thin_progress(ui: &mut egui::Ui, frac: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 4.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 2.0, Color32::from_white_alpha(30));
    let mut fill = rect;
    fill.set_width(rect.width() * frac.clamp(0.0, 1.0));
    ui.painter().rect_filled(fill, 2.0, ACCENT);
}

fn rounded_rect_path(rect: Rect, r: f32) -> Vec<Pos2> {
    let steps = 8;
    let corners = [
        (rect.right_top() + vec2(-r, r), -90.0_f32),
        (rect.right_bottom() + vec2(-r, -r), 0.0),
        (rect.left_bottom() + vec2(r, -r), 90.0),
        (rect.left_top() + vec2(r, r), 180.0),
    ];
    let mut pts = Vec::new();
    for (c, start) in corners {
        for i in 0..=steps {
            let a = (start + 90.0 * i as f32 / steps as f32).to_radians();
            pts.push(c + vec2(a.cos(), a.sin()) * r);
        }
    }
    pts.push(pts[0]);
    pts
}

fn drop_zone(ui: &mut egui::Ui, hovering: bool) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 104.0), egui::Sense::hover());
    let (fill, line) = if hovering {
        (Color32::from_rgba_unmultiplied(10, 132, 255, 40), ACCENT)
    } else {
        (Color32::from_white_alpha(8), Color32::from_white_alpha(70))
    };
    let p = ui.painter();
    p.rect_filled(rect, 12.0, fill);
    let path = rounded_rect_path(rect.shrink(0.75), 12.0);
    p.extend(egui::Shape::dashed_line(&path, Stroke::new(1.5, line), 6.0, 5.0));

    // Down arrow into a tray.
    let c = rect.center() + vec2(0.0, -20.0);
    let s = Stroke::new(2.0, if hovering { ACCENT } else { TEXT2 });
    p.line_segment([c + vec2(0.0, -9.0), c + vec2(0.0, 5.0)], s);
    p.line_segment([c + vec2(-5.0, 0.0), c + vec2(0.0, 5.0)], s);
    p.line_segment([c + vec2(5.0, 0.0), c + vec2(0.0, 5.0)], s);
    p.line_segment([c + vec2(-9.0, 10.0), c + vec2(9.0, 10.0)], s);

    p.text(
        rect.center() + vec2(0.0, 16.0),
        egui::Align2::CENTER_CENTER,
        if hovering { "Release to send" } else { "Drop files to send" },
        egui::FontId::proportional(14.0),
        if hovering { ACCENT } else { TEXT },
    );
    p.text(
        rect.center() + vec2(0.0, 34.0),
        egui::Align2::CENTER_CENTER,
        "They appear on your other PCs",
        egui::FontId::proportional(11.5),
        TEXT2,
    );
}

fn draw_kind_icon(painter: &egui::Painter, rect: Rect, kind: &ItemKind) {
    let color = match kind {
        ItemKind::File => ACCENT,
        ItemKind::Text => ORANGE,
        ItemKind::Image => PURPLE,
    };
    painter.rect_filled(rect, 9.0, color);
    let c = rect.center();
    let s = Stroke::new(1.7, Color32::WHITE);
    match kind {
        ItemKind::File => {
            painter.rect_stroke(Rect::from_center_size(c, vec2(12.0, 16.0)), 2.5, s);
            painter.line_segment([c + vec2(-3.0, -2.0), c + vec2(3.0, -2.0)], s);
            painter.line_segment([c + vec2(-3.0, 2.5), c + vec2(3.0, 2.5)], s);
        }
        ItemKind::Text => {
            painter.line_segment([c + vec2(-7.0, -5.5), c + vec2(7.0, -5.5)], s);
            painter.line_segment([c + vec2(-7.0, 0.0), c + vec2(7.0, 0.0)], s);
            painter.line_segment([c + vec2(-7.0, 5.5), c + vec2(2.0, 5.5)], s);
        }
        ItemKind::Image => {
            painter.rect_stroke(Rect::from_center_size(c, vec2(18.0, 14.0)), 3.0, s);
            painter.circle_filled(c + vec2(-3.5, -2.0), 1.8, Color32::WHITE);
            painter.add(egui::Shape::line(
                vec![c + vec2(-7.0, 5.0), c + vec2(-1.5, 0.5), c + vec2(1.5, 3.0), c + vec2(4.0, 0.5), c + vec2(7.0, 4.5)],
                s,
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

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
        apply_mac_style(&ctx);
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
    // Transparent window so the panel can have rounded corners.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Rgba::TRANSPARENT.to_array()
    }

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

        let frame = if collapsed {
            egui::Frame::none().inner_margin(egui::Margin::ZERO)
        } else {
            egui::Frame::none()
                .fill(BG)
                .rounding(Rounding { nw: 12.0, sw: 12.0, ne: 0.0, se: 0.0 })
                .stroke(Stroke::new(1.0, Color32::from_white_alpha(28)))
                .inner_margin(egui::Margin::same(14.0))
        };

        egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
            if collapsed {
                // A slim blue grabber at the screen edge.
                let rect = ui.max_rect();
                let pill = Rect::from_center_size(
                    Pos2::new(rect.right() - 6.0, rect.center().y),
                    vec2(5.0, 72.0),
                );
                ui.painter().rect_filled(pill, 2.5, ACCENT);
                return;
            }
            ui.set_min_width(EXPANDED_W - 28.0);

            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                // Header.
                ui.horizontal(|ui| {
                    if quit_dot(ui).clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    ui.label(RichText::new("Shelf").size(17.0).strong().color(TEXT));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(shorten(&self.device_name, 22)).size(11.5).color(TEXT2),
                        );
                    });
                });
                if !self.status.is_empty() {
                    ui.label(RichText::new(self.status.clone()).size(11.5).color(TEXT2));
                }
                if let Some((label, pct)) = &self.progress {
                    ui.label(
                        RichText::new(format!("{} ({}%)", shorten(label, 34), pct))
                            .size(11.5)
                            .color(TEXT2),
                    );
                    thin_progress(ui, *pct as f32 / 100.0);
                }

                // Drop zone and clipboard.
                drop_zone(ui, hovering_files);
                if ui
                    .add_sized([ui.available_width(), 30.0], primary_button("Send Clipboard"))
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    actions.push(Action::SendClipboard);
                }

                // Settings.
                let label = self.target_label();
                card(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Send to").color(TEXT));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            egui::ComboBox::from_id_source("send_to")
                                .selected_text(label)
                                .width(170.0)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut self.target,
                                        Target::All,
                                        "All paired devices",
                                    );
                                    for p in &paired {
                                        ui.selectable_value(
                                            &mut self.target,
                                            Target::Peer(p.public_key.clone()),
                                            p.name.clone(),
                                        );
                                    }
                                });
                        });
                    });
                    hairline(ui);
                    row_toggle(ui, "Auto-copy received", &mut self.auto_copy);
                    hairline(ui);
                    row_toggle(ui, "Keep open", &mut self.pinned);
                });

                // Devices and pairing.
                egui::CollapsingHeader::new(RichText::new("DEVICES").size(11.0).color(TEXT2))
                    .default_open(
                        paired.is_empty() || self.my_code.is_some() || self.pair_dialog.is_some(),
                    )
                    .show(ui, |ui| {
                        if paired.is_empty() && nearby.is_empty() {
                            ui.label(
                                RichText::new(
                                    "No devices yet. Open Shelf on your other PC, then pair below.",
                                )
                                .size(12.0)
                                .color(TEXT2),
                            );
                        }
                        if !paired.is_empty() {
                            card(ui, |ui| {
                                for (i, p) in paired.iter().enumerate() {
                                    if i > 0 {
                                        hairline(ui);
                                    }
                                    let online = p.addr.is_some();
                                    ui.horizontal(|ui| {
                                        status_dot(ui, if online { GOOD } else { TEXT3 });
                                        ui.label(RichText::new(shorten(&p.name, 22)).color(TEXT));
                                        ui.label(
                                            RichText::new(if online { "Online" } else { "Offline" })
                                                .size(11.5)
                                                .color(TEXT2),
                                        );
                                        ui.with_layout(
                                            Layout::right_to_left(Align::Center),
                                            |ui| {
                                                if link_button(ui, "Remove", RED).clicked() {
                                                    actions.push(Action::Unpair(
                                                        p.public_key.clone(),
                                                    ));
                                                }
                                            },
                                        );
                                    });
                                }
                            });
                        }
                        if !nearby.is_empty() {
                            section_label(ui, "Nearby, not paired");
                            card(ui, |ui| {
                                for (i, d) in nearby.iter().enumerate() {
                                    if i > 0 {
                                        hairline(ui);
                                    }
                                    ui.horizontal(|ui| {
                                        ui.label(RichText::new(shorten(&d.name, 24)).color(TEXT));
                                        ui.with_layout(
                                            Layout::right_to_left(Align::Center),
                                            |ui| {
                                                if ui.add(pill_button("Pair")).clicked() {
                                                    actions.push(Action::OpenPair(d.clone()));
                                                }
                                            },
                                        );
                                    });
                                }
                            });
                        }

                        if let Some((code, until)) = &self.my_code {
                            let left = until.saturating_duration_since(Instant::now()).as_secs();
                            let spaced = format!("{} {}", &code[..3], &code[3..]);
                            card(ui, |ui| {
                                ui.vertical_centered(|ui| {
                                    ui.label(
                                        RichText::new("Enter this code on the other PC")
                                            .size(12.0)
                                            .color(TEXT2),
                                    );
                                    ui.label(
                                        RichText::new(spaced)
                                            .monospace()
                                            .size(32.0)
                                            .strong()
                                            .color(TEXT),
                                    );
                                    ui.label(
                                        RichText::new(format!("Expires in {left}s"))
                                            .size(11.5)
                                            .color(TEXT3),
                                    );
                                    if link_button(ui, "Cancel", ACCENT).clicked() {
                                        actions.push(Action::CancelPairing);
                                    }
                                });
                            });
                        } else if self.pair_dialog.is_none() {
                            if ui
                                .add_sized(
                                    [ui.available_width(), 28.0],
                                    egui::Button::new("Show a pairing code on this PC"),
                                )
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .clicked()
                            {
                                actions.push(Action::StartPairing);
                            }
                        }

                        if let Some(d) = self.pair_dialog.as_mut() {
                            card(ui, |ui| {
                                ui.label(
                                    RichText::new(format!("Pair with {}", shorten(&d.device.name, 24)))
                                        .color(TEXT),
                                );
                                ui.label(
                                    RichText::new("Type the 6 digit code shown on that PC.")
                                        .size(12.0)
                                        .color(TEXT2),
                                );
                                ui.add(
                                    egui::TextEdit::singleline(&mut d.code)
                                        .font(egui::FontId::monospace(20.0))
                                        .horizontal_align(Align::Center)
                                        .desired_width(ui.available_width())
                                        .char_limit(6)
                                        .hint_text("000000"),
                                );
                                if let Some(e) = &d.error {
                                    ui.label(RichText::new(e.clone()).size(12.0).color(RED));
                                }
                                ui.horizontal(|ui| {
                                    let valid =
                                        d.code.len() == 6 && d.code.chars().all(|c| c.is_ascii_digit());
                                    if ui
                                        .add_enabled(valid && !d.busy, primary_button("Connect"))
                                        .clicked()
                                    {
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

                // The shelf itself.
                section_label(ui, "On the shelf");
                if self.items.is_empty() {
                    card(ui, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.add_space(8.0);
                            ui.label(RichText::new("Nothing on the shelf").color(TEXT));
                            ui.label(
                                RichText::new("Items you receive show up here for an hour.")
                                    .size(12.0)
                                    .color(TEXT2),
                            );
                            ui.add_space(8.0);
                        });
                    });
                }
                for (i, it) in self.items.iter().enumerate() {
                    card(ui, |ui| {
                        ui.horizontal(|ui| {
                            let (icon_rect, _) =
                                ui.allocate_exact_size(vec2(36.0, 36.0), egui::Sense::hover());
                            draw_kind_icon(ui.painter(), icon_rect, &it.item.kind);
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                ui.label(
                                    RichText::new(shorten(&it.item.name, 28))
                                        .size(13.5)
                                        .strong()
                                        .color(TEXT),
                                );
                                if let Some(t) = &it.item.text {
                                    ui.label(
                                        RichText::new(shorten(&t.replace('\n', " "), 44))
                                            .size(12.0)
                                            .color(TEXT2),
                                    );
                                }
                                let mins =
                                    ITEM_TTL.saturating_sub(it.received.elapsed()).as_secs() / 60;
                                ui.label(
                                    RichText::new(format!(
                                        "{} \u{00B7} {} \u{00B7} {} min left",
                                        shorten(&it.item.from, 14),
                                        human_size(it.item.size),
                                        mins
                                    ))
                                    .size(11.5)
                                    .color(TEXT3),
                                );
                            });
                        });
                        ui.horizontal(|ui| {
                            ui.add_space(36.0);
                            if link_button(ui, "Copy", ACCENT).clicked() {
                                actions.push(Action::Copy(i));
                            }
                            if it.item.path.is_some()
                                && link_button(ui, "Show", ACCENT).clicked()
                            {
                                actions.push(Action::Reveal(i));
                            }
                            if link_button(ui, "Remove", RED).clicked() {
                                actions.push(Action::Remove(i));
                            }
                        });
                    });
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

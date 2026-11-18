#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod clip;
mod monitors;

use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, FontFamily, FontId, Layout, Pos2, Rect, RichText,
    Rounding, Stroke, Vec2,
};
use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
};
use shelf_core::{
    Command, DeviceGroup, Event, Handle, ItemKind, NearbyDevice, Payload, PeerStatus, ReceivedItem,
    Room, RoomRole, RoomUpdate, Target, TransferDirection,
};
use std::cell::Cell;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

const COLLAPSED_W: f32 = 14.0;
const EXPANDED_W: f32 = 430.0;
const ITEM_TTL: Duration = Duration::from_secs(3600);

// Requests coming from the tray icon (set on another thread, read in `update`).
static TRAY_SHOW: AtomicBool = AtomicBool::new(false);
static TRAY_QUIT: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Side {
    Left,
    Right,
}

/// Small preferences kept in `settings.txt` (one `key=value` per line).
#[derive(Clone, Copy, PartialEq, Debug)]
struct Settings {
    side: Side,
    monitor: usize,
    auto_copy: bool,
    pinned: bool,
    /// Material 3 look instead of the macOS look.
    material: bool,
}

impl Settings {
    fn path() -> PathBuf {
        shelf_core::data_dir().join("settings.txt")
    }

    fn load() -> Settings {
        let mut s = Settings {
            side: Side::Right,
            monitor: usize::MAX,
            auto_copy: true,
            pinned: false,
            material: false,
        };
        if let Ok(text) = std::fs::read_to_string(Self::path()) {
            for line in text.lines() {
                let Some((k, v)) = line.split_once('=') else {
                    continue;
                };
                match k.trim() {
                    "side" => {
                        s.side = if v.trim() == "left" {
                            Side::Left
                        } else {
                            Side::Right
                        }
                    }
                    "monitor" => s.monitor = v.trim().parse().unwrap_or(usize::MAX),
                    "auto_copy" => s.auto_copy = v.trim() != "0",
                    "pinned" => s.pinned = v.trim() == "1",
                    "style" => s.material = v.trim() == "material",
                    _ => {}
                }
            }
        }
        s
    }

    fn save(&self) {
        let text = format!(
            "side={}\nmonitor={}\nauto_copy={}\npinned={}\nstyle={}\n",
            if self.side == Side::Left {
                "left"
            } else {
                "right"
            },
            if self.monitor == usize::MAX {
                -1
            } else {
                self.monitor as i64
            },
            if self.auto_copy { 1 } else { 0 },
            if self.pinned { 1 } else { 0 },
            if self.material { "material" } else { "macos" },
        );
        let _ = std::fs::create_dir_all(shelf_core::data_dir());
        let _ = std::fs::write(Self::path(), text);
    }
}

/// Tray icon with "Show Shelf" and "Quit Shelf". Must be created on the UI thread.
fn build_tray(ctx: &egui::Context) -> Option<TrayIcon> {
    let data = load_icon()?;
    let icon = Icon::from_rgba(data.rgba, data.width, data.height).ok()?;
    let menu = Menu::new();
    let show = MenuItem::new("Show Shelf", true, None);
    let quit = MenuItem::new("Quit Shelf", true, None);
    menu.append(&show).ok()?;
    menu.append(&PredefinedMenuItem::separator()).ok()?;
    menu.append(&quit).ok()?;
    let show_id = show.id().clone();
    let quit_id = quit.id().clone();

    let menu_ctx = ctx.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if event.id == show_id {
            TRAY_SHOW.store(true, Ordering::SeqCst);
        } else if event.id == quit_id {
            TRAY_QUIT.store(true, Ordering::SeqCst);
        }
        menu_ctx.request_repaint();
    }));
    let click_ctx = ctx.clone();
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = event
        {
            TRAY_SHOW.store(true, Ordering::SeqCst);
            click_ctx.request_repaint();
        }
    }));

    TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("Shelf")
        .with_icon(icon)
        .build()
        .ok()
}

fn register_hotkeys() -> Option<(GlobalHotKeyManager, u32, u32)> {
    let manager = GlobalHotKeyManager::new().ok()?;
    let toggle = HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyS);
    let send_clipboard = HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyC);
    manager.register(toggle).ok()?;
    manager.register(send_clipboard).ok()?;
    Some((manager, toggle.id(), send_clipboard.id()))
}

#[derive(Clone, Copy)]
enum Locale {
    English,
    French,
    Spanish,
    German,
}

static LOCALE: OnceLock<Locale> = OnceLock::new();

fn system_locale() -> Locale {
    *LOCALE.get_or_init(|| {
        let mut value = std::env::var("LANG").unwrap_or_default();
        if value.is_empty() {
            value = std::process::Command::new("powershell.exe")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "[System.Globalization.CultureInfo]::CurrentUICulture.Name",
                ])
                .output()
                .ok()
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
                .unwrap_or_default();
        }
        let value = value.to_ascii_lowercase();
        if value.starts_with("fr") {
            Locale::French
        } else if value.starts_with("es") {
            Locale::Spanish
        } else if value.starts_with("de") {
            Locale::German
        } else {
            Locale::English
        }
    })
}

fn tr(key: &'static str) -> &'static str {
    match (system_locale(), key) {
        (Locale::French, "Send Clipboard") => "Envoyer le presse-papiers",
        (Locale::Spanish, "Send Clipboard") => "Enviar portapapeles",
        (Locale::German, "Send Clipboard") => "Zwischenablage senden",
        (Locale::French, "Phone upload") => "Envoi depuis le téléphone",
        (Locale::Spanish, "Phone upload") => "Envío desde el móvil",
        (Locale::German, "Phone upload") => "Upload vom Handy",
        (Locale::French, "Send from your phone") => "Envoyer depuis votre téléphone",
        (Locale::Spanish, "Send from your phone") => "Enviar desde tu móvil",
        (Locale::German, "Send from your phone") => "Vom Handy senden",
        (Locale::French, "Scan with the phone camera. Both devices need the same Wi-Fi.") => {
            "Scannez avec l'appareil photo du téléphone. Les deux appareils doivent être sur le même Wi-Fi."
        }
        (Locale::Spanish, "Scan with the phone camera. Both devices need the same Wi-Fi.") => {
            "Escanea con la cámara del móvil. Ambos dispositivos deben estar en la misma red Wi-Fi."
        }
        (Locale::German, "Scan with the phone camera. Both devices need the same Wi-Fi.") => {
            "Mit der Handykamera scannen. Beide Geräte müssen im selben WLAN sein."
        }
        (Locale::French, "Closes in") => "Se ferme dans",
        (Locale::Spanish, "Closes in") => "Se cierra en",
        (Locale::German, "Closes in") => "Schließt in",
        (Locale::French, "Copy link") => "Copier le lien",
        (Locale::Spanish, "Copy link") => "Copiar enlace",
        (Locale::German, "Copy link") => "Link kopieren",
        (Locale::French, "Stop") => "Arrêter",
        (Locale::Spanish, "Stop") => "Detener",
        (Locale::German, "Stop") => "Beenden",
        (Locale::French, "Not encrypted. Use it on a network you trust.") => {
            "Non chiffré. À utiliser sur un réseau de confiance."
        }
        (Locale::Spanish, "Not encrypted. Use it on a network you trust.") => {
            "Sin cifrar. Úsalo en una red de confianza."
        }
        (Locale::German, "Not encrypted. Use it on a network you trust.") => {
            "Nicht verschlüsselt. Nur in einem vertrauenswürdigen Netzwerk nutzen."
        }
        (Locale::French, "Drop files") => "Déposer des fichiers",
        (Locale::Spanish, "Drop files") => "Suelta archivos",
        (Locale::German, "Drop files") => "Dateien ablegen",
        (Locale::French, "Drag here to send") => "Glisser ici pour envoyer",
        (Locale::Spanish, "Drag here to send") => "Arrastra aquí para enviar",
        (Locale::German, "Drag here to send") => "Zum Senden hierher ziehen",
        (Locale::French, "Clipboard") => "Presse-papiers",
        (Locale::Spanish, "Clipboard") => "Portapapeles",
        (Locale::German, "Clipboard") => "Zwischenablage",
        (Locale::French, "Send") => "Envoyer",
        (Locale::Spanish, "Send") => "Enviar",
        (Locale::German, "Send") => "Senden",
        (Locale::French, "online") => "en ligne",
        (Locale::Spanish, "online") => "en línea",
        (Locale::French, "Transfers") => "Transferts",
        (Locale::Spanish, "Transfers") => "Transferencias",
        (Locale::German, "Transfers") => "Übertragungen",
        (Locale::French, "active") => "actifs",
        (Locale::Spanish, "active") => "activas",
        (Locale::German, "active") => "aktiv",
        (Locale::French, "All settings") => "Tous les réglages",
        (Locale::Spanish, "All settings") => "Todos los ajustes",
        (Locale::German, "All settings") => "Alle Einstellungen",
        (Locale::French, "Hide settings") => "Masquer les réglages",
        (Locale::Spanish, "Hide settings") => "Ocultar ajustes",
        (Locale::German, "Hide settings") => "Einstellungen ausblenden",
        (Locale::French, "Send to") => "Envoyer à",
        (Locale::Spanish, "Send to") => "Enviar a",
        (Locale::German, "Send to") => "Senden an",
        (Locale::French, "All devices") => "Tous les appareils",
        (Locale::Spanish, "All devices") => "Todos los dispositivos",
        (Locale::German, "All devices") => "Alle Geräte",
        (Locale::French, "Rooms") => "Espaces",
        (Locale::Spanish, "Rooms") => "Salas",
        (Locale::German, "Rooms") => "Räume",
        (Locale::French, "Devices") => "Appareils",
        (Locale::Spanish, "Devices") => "Dispositivos",
        (Locale::German, "Devices") => "Geräte",
        (Locale::French, "Activity") => "Activité",
        (Locale::Spanish, "Activity") => "Actividad",
        (Locale::German, "Activity") => "Aktivität",
        (Locale::French, "Create") => "Créer",
        (Locale::Spanish, "Create") => "Crear",
        (Locale::German, "Create") => "Erstellen",
        (Locale::French, "Request") => "Demander",
        (Locale::Spanish, "Request") => "Solicitar",
        (Locale::German, "Request") => "Anfordern",
        (Locale::French, "Submit") => "Remettre",
        (Locale::Spanish, "Submit") => "Entregar",
        (Locale::German, "Submit") => "Abgeben",
        (Locale::French, "Announce") => "Annoncer",
        (Locale::Spanish, "Announce") => "Anunciar",
        (Locale::German, "Announce") => "Ankündigen",
        (Locale::French, "Share text") => "Partager le texte",
        (Locale::Spanish, "Share text") => "Compartir texto",
        (Locale::German, "Share text") => "Text teilen",
        (Locale::French, "Copy") => "Copier",
        (Locale::Spanish, "Copy") => "Copiar",
        (Locale::German, "Copy") => "Kopieren",
        (Locale::French, "Remove") => "Supprimer",
        (Locale::Spanish, "Remove") => "Eliminar",
        (Locale::German, "Remove") => "Entfernen",
        (Locale::French, "Lock") => "Verrouiller",
        (Locale::Spanish, "Lock") => "Bloquear",
        (Locale::German, "Lock") => "Sperren",
        (Locale::French, "Unlock") => "Déverrouiller",
        (Locale::Spanish, "Unlock") => "Desbloquear",
        (Locale::German, "Unlock") => "Entsperren",
        (Locale::French, "Delete") => "Supprimer",
        (Locale::Spanish, "Delete") => "Eliminar",
        (Locale::German, "Delete") => "Löschen",
        (Locale::French, "Cancel") => "Annuler",
        (Locale::Spanish, "Cancel") => "Cancelar",
        (Locale::German, "Cancel") => "Abbrechen",
        (Locale::French, "Join") => "Rejoindre",
        (Locale::Spanish, "Join") => "Unirse",
        (Locale::German, "Join") => "Beitreten",
        (Locale::French, "Start pairing") => "Commencer l'association",
        (Locale::Spanish, "Start pairing") => "Iniciar emparejamiento",
        (Locale::German, "Start pairing") => "Kopplung starten",
        (Locale::French, "Pair") => "Associer",
        (Locale::Spanish, "Pair") => "Emparejar",
        (Locale::German, "Pair") => "Koppeln",
        (Locale::French, "Connect") => "Connecter",
        (Locale::Spanish, "Connect") => "Conectar",
        (Locale::German, "Connect") => "Verbinden",
        (Locale::French, "Show pairing code") => "Afficher le code d'association",
        (Locale::Spanish, "Show pairing code") => "Mostrar código de emparejamiento",
        (Locale::German, "Show pairing code") => "Kopplungscode anzeigen",
        (Locale::French, "Nearby") => "À proximité",
        (Locale::Spanish, "Nearby") => "Cercanos",
        (Locale::German, "Nearby") => "In der Nähe",
        (Locale::French, "Paired") => "Associés",
        (Locale::Spanish, "Paired") => "Emparejados",
        (Locale::German, "Paired") => "Gekoppelt",
        (Locale::French, "Not paired") => "Non associé",
        (Locale::Spanish, "Not paired") => "No emparejado",
        (Locale::German, "Not paired") => "Nicht gekoppelt",
        (Locale::French, "No activity yet.") => "Aucune activité pour le moment.",
        (Locale::Spanish, "No activity yet.") => "Aún no hay actividad.",
        (Locale::German, "No activity yet.") => "Noch keine Aktivität.",
        (Locale::French, "New class or team room") => "Nouvel espace de classe ou d'équipe",
        (Locale::Spanish, "New class or team room") => "Nueva sala de clase o equipo",
        (Locale::German, "New class or team room") => "Neuer Klassen- oder Teamraum",
        (Locale::French, "Request a file from the room") => "Demander un fichier à l'espace",
        (Locale::Spanish, "Request a file from the room") => "Solicitar un archivo a la sala",
        (Locale::German, "Request a file from the room") => "Datei aus dem Raum anfordern",
        (Locale::French, "Team announcement") => "Annonce de l'équipe",
        (Locale::Spanish, "Team announcement") => "Anuncio del equipo",
        (Locale::German, "Team announcement") => "Teamankündigung",
        (Locale::French, "Shared clipboard entry") => "Entrée du presse-papiers partagé",
        (Locale::Spanish, "Shared clipboard entry") => "Entrada del portapapeles compartido",
        (Locale::German, "Shared clipboard entry") => "Gemeinsamer Zwischenablageeintrag",
        (Locale::French, "Add note") => "Ajouter une note",
        (Locale::Spanish, "Add note") => "Añadir nota",
        (Locale::German, "Add note") => "Notiz hinzufügen",
        (Locale::French, "Add link") => "Ajouter un lien",
        (Locale::Spanish, "Add link") => "Añadir enlace",
        (Locale::German, "Add link") => "Link hinzufügen",
        (Locale::French, "Set expiry") => "Définir l'expiration",
        (Locale::Spanish, "Set expiry") => "Establecer vencimiento",
        (Locale::German, "Set expiry") => "Ablauf festlegen",
        (Locale::French, "Announcements") => "Annonces",
        (Locale::Spanish, "Announcements") => "Anuncios",
        (Locale::German, "Announcements") => "Ankündigungen",
        (Locale::French, "Shared clipboard") => "Presse-papiers partagé",
        (Locale::Spanish, "Shared clipboard") => "Portapapeles compartido",
        (Locale::German, "Shared clipboard") => "Gemeinsame Zwischenablage",
        (Locale::French, "Recent activity") => "Activité récente",
        (Locale::Spanish, "Recent activity") => "Actividad reciente",
        (Locale::German, "Recent activity") => "Letzte Aktivität",
        (Locale::French, "Shared notes") => "Notes partagées",
        (Locale::Spanish, "Shared notes") => "Notas compartidas",
        (Locale::German, "Shared notes") => "Gemeinsame Notizen",
        (Locale::French, "Shared links") => "Liens partagés",
        (Locale::Spanish, "Shared links") => "Enlaces compartidos",
        (Locale::German, "Shared links") => "Geteilte Links",
        (Locale::French, "Members and roles") => "Membres et rôles",
        (Locale::Spanish, "Members and roles") => "Miembros y roles",
        (Locale::German, "Members and roles") => "Mitglieder und Rollen",
        (Locale::French, "Teacher assignment") => "Devoir de l'enseignant",
        (Locale::Spanish, "Teacher assignment") => "Tarea del profesor",
        (Locale::German, "Teacher assignment") => "Aufgabe der Lehrkraft",
        (Locale::French, "Create assignment") => "Créer le devoir",
        (Locale::Spanish, "Create assignment") => "Crear tarea",
        (Locale::German, "Create assignment") => "Aufgabe erstellen",
        (Locale::French, "Nothing on the shelf") => "Rien sur l'étagère",
        (Locale::Spanish, "Nothing on the shelf") => "Nada en el estante",
        (Locale::German, "Nothing on the shelf") => "Nichts im Shelf",
        _ => key,
    }
}

// The window is transparent; the glass panel floats inside it with this gap on every side,
// which also leaves room for the soft shadow.
const GAP: f32 = 14.0;
const PANEL_RADIUS: f32 = 36.0;
const CARD_RADIUS: f32 = 24.0;

// ---------------------------------------------------------------------------
// Palette (macOS Tahoe "Liquid Glass" inspired, light and dark)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Pal {
    dark: bool,
    /// True for the Material 3 look.
    m3: bool,
    /// Text and icons drawn on top of `accent` (white on macOS blue, dark on M3 dark primary).
    on_accent: Color32,
    panel: Color32,
    panel_edge: Color32,
    card: Color32,
    card_edge: Color32,
    field: Color32,
    text: Color32,
    text2: Color32,
    text3: Color32,
    accent: Color32,
    good: Color32,
    red: Color32,
    orange: Color32,
    purple: Color32,
    sep: Color32,
    fill: Color32,
    fill_hover: Color32,
    fill_press: Color32,
    /// Round icon wells (the dark disc behind a glyph).
    well: Color32,
    /// Tonal highlight: selected chips, active quick actions, the drop well.
    tonal: Color32,
}

impl Pal {
    fn dark() -> Self {
        Self {
            dark: true,
            m3: false,
            on_accent: Color32::WHITE,
            // Tinted, see-through blue glass.
            panel: Color32::from_rgba_unmultiplied(26, 48, 98, 176),
            panel_edge: Color32::from_white_alpha(78),
            card: Color32::from_white_alpha(40),
            card_edge: Color32::from_white_alpha(68),
            field: Color32::from_rgba_unmultiplied(8, 20, 56, 90),
            text: Color32::from_rgb(248, 250, 255),
            text2: Color32::from_rgb(196, 212, 238),
            text3: Color32::from_rgb(146, 166, 202),
            accent: Color32::from_rgb(52, 120, 246),
            good: Color32::from_rgb(48, 209, 88),
            red: Color32::from_rgb(255, 99, 88),
            orange: Color32::from_rgb(255, 169, 40),
            purple: Color32::from_rgb(191, 110, 250),
            sep: Color32::from_white_alpha(40),
            fill: Color32::from_white_alpha(40),
            fill_hover: Color32::from_white_alpha(58),
            fill_press: Color32::from_white_alpha(76),
            well: Color32::from_rgba_unmultiplied(6, 40, 84, 128),
            tonal: Color32::from_rgb(52, 120, 246),
        }
    }

    fn light() -> Self {
        Self {
            dark: false,
            m3: false,
            on_accent: Color32::WHITE,
            panel: Color32::from_rgba_unmultiplied(226, 238, 255, 196),
            panel_edge: Color32::from_white_alpha(210),
            card: Color32::from_white_alpha(120),
            card_edge: Color32::from_white_alpha(190),
            field: Color32::from_white_alpha(190),
            text: Color32::from_rgb(18, 32, 66),
            text2: Color32::from_rgb(70, 90, 130),
            text3: Color32::from_rgb(120, 138, 172),
            accent: Color32::from_rgb(0, 112, 245),
            good: Color32::from_rgb(40, 205, 65),
            red: Color32::from_rgb(255, 59, 48),
            orange: Color32::from_rgb(255, 149, 0),
            purple: Color32::from_rgb(175, 82, 222),
            sep: Color32::from_rgba_unmultiplied(40, 70, 130, 36),
            fill: Color32::from_white_alpha(120),
            fill_hover: Color32::from_white_alpha(160),
            fill_press: Color32::from_white_alpha(200),
            well: Color32::from_rgba_unmultiplied(40, 80, 150, 80),
            tonal: Color32::from_rgb(0, 112, 245),
        }
    }

    /// Material 3 with a soft sage-green seed, dark scheme.
    fn m3_dark() -> Self {
        Self {
            dark: true,
            m3: true,
            on_accent: Color32::from_rgb(0, 56, 34),
            panel: Color32::from_rgb(30, 34, 32),
            panel_edge: Color32::TRANSPARENT,
            card: Color32::from_rgb(40, 45, 42),
            card_edge: Color32::TRANSPARENT,
            field: Color32::from_rgb(50, 56, 52),
            text: Color32::from_rgb(228, 233, 230),
            text2: Color32::from_rgb(170, 178, 173),
            text3: Color32::from_rgb(125, 133, 128),
            accent: Color32::from_rgb(146, 208, 170),
            good: Color32::from_rgb(146, 208, 170),
            red: Color32::from_rgb(255, 180, 171),
            orange: Color32::from_rgb(255, 183, 77),
            purple: Color32::from_rgb(214, 186, 255),
            sep: Color32::from_rgb(60, 67, 63),
            fill: Color32::from_rgb(48, 54, 51),
            fill_hover: Color32::from_rgb(58, 65, 61),
            fill_press: Color32::from_rgb(66, 73, 69),
            well: Color32::from_rgb(28, 32, 30),
            tonal: Color32::from_rgb(52, 84, 66),
        }
    }

    /// Material 3 with a soft sage-green seed, light scheme.
    fn m3_light() -> Self {
        Self {
            dark: false,
            m3: true,
            on_accent: Color32::WHITE,
            panel: Color32::from_rgb(232, 236, 233),
            panel_edge: Color32::TRANSPARENT,
            card: Color32::from_rgb(241, 244, 242),
            card_edge: Color32::TRANSPARENT,
            field: Color32::from_rgb(226, 230, 227),
            text: Color32::from_rgb(24, 28, 26),
            text2: Color32::from_rgb(92, 100, 96),
            text3: Color32::from_rgb(135, 142, 138),
            accent: Color32::from_rgb(58, 108, 82),
            good: Color32::from_rgb(58, 108, 82),
            red: Color32::from_rgb(186, 26, 26),
            orange: Color32::from_rgb(180, 95, 0),
            purple: Color32::from_rgb(110, 57, 200),
            sep: Color32::from_rgb(206, 211, 208),
            fill: Color32::from_rgb(225, 229, 226),
            fill_hover: Color32::from_rgb(216, 221, 218),
            fill_press: Color32::from_rgb(206, 212, 208),
            well: Color32::from_rgb(224, 229, 226),
            tonal: Color32::from_rgb(200, 226, 209),
        }
    }
}

/// Colour field behind the macOS glass panel: the soft blue / teal wallpaper blur of the mockup,
/// with a warm glow on the left edge and a pale patch low on the right. Built as a mesh whose
/// outline follows the panel's rounded corners, so the colours blend smoothly.
fn glass_backdrop(painter: &egui::Painter, rect: Rect, radius: f32, dark: bool) {
    const COLS: usize = 5;
    const ROWS: usize = 8;
    const GRID: [[(u8, u8, u8); COLS]; ROWS] = [
        [(165, 192, 205), (112, 170, 205), (60, 160, 215), (52, 168, 212), (95, 182, 190)],
        [(150, 188, 196), (82, 170, 205), (58, 165, 212), (68, 170, 205), (88, 178, 192)],
        [(172, 172, 125), (92, 165, 192), (40, 132, 215), (34, 126, 220), (60, 152, 215)],
        [(82, 142, 190), (30, 102, 205), (14, 72, 200), (30, 120, 215), (45, 160, 215)],
        [(15, 82, 200), (10, 72, 205), (30, 112, 210), (40, 150, 210), (50, 160, 210)],
        [(20, 100, 210), (30, 110, 200), (70, 150, 200), (120, 176, 205), (150, 186, 205)],
        [(30, 110, 210), (40, 130, 215), (60, 150, 220), (100, 176, 225), (140, 190, 225)],
        [(40, 130, 215), (30, 120, 215), (36, 126, 220), (50, 140, 215), (60, 150, 210)],
    ];
    let smooth = |t: f32| t * t * (3.0 - 2.0 * t);
    let sample = |u: f32, v: f32| -> Color32 {
        let gx = u.clamp(0.0, 1.0) * (COLS - 1) as f32;
        let gy = v.clamp(0.0, 1.0) * (ROWS - 1) as f32;
        let (j, i) = ((gx as usize).min(COLS - 2), (gy as usize).min(ROWS - 2));
        let (fx, fy) = (smooth(gx - j as f32), smooth(gy - i as f32));
        let mut rgb = [0.0_f32; 3];
        for (k, out) in rgb.iter_mut().enumerate() {
            let at = |r: usize, c: usize| {
                let (r8, g8, b8) = GRID[r][c];
                [r8, g8, b8][k] as f32
            };
            let top = at(i, j) * (1.0 - fx) + at(i, j + 1) * fx;
            let bottom = at(i + 1, j) * (1.0 - fx) + at(i + 1, j + 1) * fx;
            *out = top * (1.0 - fy) + bottom * fy;
        }
        if !dark {
            // Light mode: the same wallpaper, washed out towards white.
            for c in rgb.iter_mut() {
                *c = *c * 0.5 + 244.0 * 0.5;
            }
        }
        Color32::from_rgba_unmultiplied(rgb[0] as u8, rgb[1] as u8, rgb[2] as u8, 240)
    };

    let r = radius.min(rect.height() / 2.0).min(rect.width() / 2.0);
    let corner_steps = 10;
    let mut ys: Vec<f32> = Vec::new();
    for i in 0..=corner_steps {
        let a = std::f32::consts::FRAC_PI_2 * i as f32 / corner_steps as f32;
        ys.push(rect.top() + r * (1.0 - a.cos()));
    }
    let (mid_top, mid_bottom) = (rect.top() + r, rect.bottom() - r);
    let mid_rows = (((mid_bottom - mid_top) / 36.0).ceil() as usize).max(1);
    for i in 1..mid_rows {
        ys.push(mid_top + (mid_bottom - mid_top) * i as f32 / mid_rows as f32);
    }
    for i in (0..=corner_steps).rev() {
        let a = std::f32::consts::FRAC_PI_2 * i as f32 / corner_steps as f32;
        ys.push(rect.bottom() - r * (1.0 - a.cos()));
    }

    let cols = 16;
    let mut mesh = egui::Mesh::default();
    for &y in &ys {
        let edge_dy = (y - rect.top()).min(rect.bottom() - y);
        let inset = if edge_dy < r {
            r - (r * r - (r - edge_dy) * (r - edge_dy)).max(0.0).sqrt()
        } else {
            0.0
        };
        let (x0, x1) = (rect.left() + inset, rect.right() - inset);
        for j in 0..=cols {
            let x = x0 + (x1 - x0) * j as f32 / cols as f32;
            let u = (x - rect.left()) / rect.width().max(1.0);
            let v = (y - rect.top()) / rect.height().max(1.0);
            mesh.colored_vertex(Pos2::new(x, y), sample(u, v));
        }
    }
    let stride = (cols + 1) as u32;
    for row in 0..(ys.len() as u32 - 1) {
        for j in 0..cols as u32 {
            let a = row * stride + j;
            let (b, c, d) = (a + 1, a + stride, a + stride + 1);
            mesh.add_triangle(a, b, c);
            mesh.add_triangle(b, d, c);
        }
    }
    painter.add(egui::Shape::mesh(mesh));
}

fn panel_radius() -> f32 {
    if pal().m3 {
        40.0
    } else {
        PANEL_RADIUS
    }
}

fn card_radius() -> f32 {
    if pal().m3 {
        28.0
    } else {
        CARD_RADIUS
    }
}

thread_local! {
    static PAL: Cell<Pal> = Cell::new(Pal::dark());
}

fn pal() -> Pal {
    PAL.with(|p| p.get())
}

fn set_pal(p: Pal) {
    PAL.with(|c| c.set(p));
}

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

/// The header logo as a small texture. It is decoded with the `png` crate and shrunk with a
/// box filter, so no egui image-loader plugin is needed (without one, `egui::Image::from_bytes`
/// shows a warning triangle instead of the picture).
fn load_logo(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    const OUT: usize = 128;
    let icon = load_icon()?;
    let (w, h) = (icon.width as usize, icon.height as usize);
    if w == 0 || h == 0 || icon.rgba.len() < w * h * 4 {
        return None;
    }
    let mut out = vec![0u8; OUT * OUT * 4];
    for oy in 0..OUT {
        let y0 = oy * h / OUT;
        let y1 = ((oy + 1) * h / OUT).clamp(y0 + 1, h);
        for ox in 0..OUT {
            let x0 = ox * w / OUT;
            let x1 = ((ox + 1) * w / OUT).clamp(x0 + 1, w);
            let (mut r, mut g, mut b, mut a, mut n) = (0u64, 0u64, 0u64, 0u64, 0u64);
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = (y * w + x) * 4;
                    let alpha = icon.rgba[i + 3] as u64;
                    r += icon.rgba[i] as u64 * alpha;
                    g += icon.rgba[i + 1] as u64 * alpha;
                    b += icon.rgba[i + 2] as u64 * alpha;
                    a += alpha;
                    n += 1;
                }
            }
            if a > 0 {
                let o = (oy * OUT + ox) * 4;
                out[o] = r.checked_div(a).unwrap_or(0) as u8;
                out[o + 1] = g.checked_div(a).unwrap_or(0) as u8;
                out[o + 2] = b.checked_div(a).unwrap_or(0) as u8;
                out[o + 3] = (a / n) as u8;
            }
        }
    }
    let image = egui::ColorImage::from_rgba_unmultiplied([OUT, OUT], &out);
    Some(ctx.load_texture("shelf-logo", image, egui::TextureOptions::LINEAR))
}

fn pdf_preview(ctx: &egui::Context, id: u64) -> egui::TextureHandle {
    const W: usize = 96;
    const H: usize = 124;
    let mut rgba = vec![0u8; W * H * 4];
    for pixel in rgba.as_chunks_mut::<4>().0 {
        pixel.copy_from_slice(&[248, 248, 250, 255]);
    }
    let fill = |rgba: &mut [u8], x0: usize, y0: usize, x1: usize, y1: usize, color: [u8; 4]| {
        for y in y0..y1.min(H) {
            for x in x0..x1.min(W) {
                rgba[(y * W + x) * 4..(y * W + x) * 4 + 4].copy_from_slice(&color);
            }
        }
    };
    fill(&mut rgba, 8, 8, W - 8, H - 8, [255, 255, 255, 255]);
    fill(&mut rgba, 8, 8, W - 8, 35, [211, 47, 47, 255]);
    fill(&mut rgba, 20, 52, W - 20, 56, [190, 190, 198, 255]);
    fill(&mut rgba, 20, 64, W - 14, 68, [215, 215, 220, 255]);
    fill(&mut rgba, 20, 76, W - 26, 80, [215, 215, 220, 255]);
    fill(&mut rgba, 20, 88, W - 18, 92, [215, 215, 220, 255]);
    ctx.load_texture(
        format!("pdf-preview-{id}"),
        egui::ColorImage::from_rgba_unmultiplied([W, H], &rgba),
        egui::TextureOptions::LINEAR,
    )
}

fn load_item_preview(ctx: &egui::Context, item: &ReceivedItem) -> Option<egui::TextureHandle> {
    let path = item.path.as_deref()?;
    if item.kind == ItemKind::Image {
        let image = image::ImageReader::open(path).ok()?.decode().ok()?;
        let thumb = image.thumbnail(128, 96).to_rgba8();
        let size = [thumb.width() as usize, thumb.height() as usize];
        return Some(ctx.load_texture(
            format!("item-preview-{}", item.id),
            egui::ColorImage::from_rgba_unmultiplied(size, thumb.as_raw()),
            egui::TextureOptions::LINEAR,
        ));
    }
    if path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
    {
        return Some(pdf_preview(ctx, item.id));
    }
    None
}

fn send_to_running_instance(paths: &[PathBuf]) -> bool {
    for _ in 0..10 {
        if let Ok(mut stream) = std::net::TcpStream::connect("127.0.0.1:47831") {
            let _ = stream.write_all(b"SHELF_SEND\n");
            for path in paths {
                let _ = stream.write_all(path.to_string_lossy().as_bytes());
                let _ = stream.write_all(b"\n");
            }
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Older installers registered the Explorer verb as `shelf.exe --send %*`. Explorer does not
/// expand `%*` for a shell verb, so Shelf received the literal text and sent nothing. If the verb
/// is installed, rewrite it to the working form (one launch per selected item, path in `%1`).
#[cfg(windows)]
fn repair_explorer_verb() {
    use std::os::windows::process::CommandExt;
    const NO_WINDOW: u32 = 0x0800_0000;
    let Ok(exe) = std::env::current_exe() else { return };
    let exe = exe.to_string_lossy().to_string();
    let run = |args: &[&str]| {
        std::process::Command::new("reg")
            .args(args)
            .creation_flags(NO_WINDOW)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    };
    let verb = "Software\\Classes\\Directory\\shell\\Send with Shelf";
    if !run(&["query", &format!("HKCU\\{verb}\\command")]) {
        return; // Not installed through the installer; leave the registry alone.
    }
    let command = format!("\"{exe}\" --send \"%1\"");
    for class in ["*", "Directory"] {
        let key = format!("HKCU\\Software\\Classes\\{class}\\shell\\Send with Shelf");
        run(&["add", &key, "/v", "MultiSelectModel", "/d", "Document", "/f"]);
        run(&["add", &format!("{key}\\command"), "/ve", "/d", &command, "/f"]);
    }
}

fn main() -> eframe::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let send_paths: Vec<PathBuf> = if args.next().as_deref() == Some(std::ffi::OsStr::new("--send"))
    {
        args.map(PathBuf::from)
            .filter(|path| path.exists())
            .collect()
    } else {
        Vec::new()
    };

    // Explorer invokes a new process for a shell verb. Forward its paths to the
    // already-running Shelf instance instead of starting a second engine.
    let single_instance = match std::net::TcpListener::bind("127.0.0.1:47831") {
        Ok(listener) => listener,
        Err(_) => {
            if !send_paths.is_empty() {
                let _ = send_to_running_instance(&send_paths);
            }
            return Ok(());
        }
    };
    #[cfg(windows)]
    std::thread::spawn(repair_explorer_verb);
    let (control_tx, control_rx) = channel();
    std::thread::spawn(move || {
        for stream in single_instance.incoming().flatten() {
            let mut input = String::new();
            if stream.take(1024 * 1024).read_to_string(&mut input).is_ok() {
                let mut lines = input.lines();
                if lines.next() == Some("SHELF_SEND") {
                    let paths = lines
                        .map(PathBuf::from)
                        .filter(|path| path.exists())
                        .collect();
                    let _ = control_tx.send(paths);
                }
            }
        }
    });

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
        Box::new(move |cc| {
            let mut app = ShelfApp::new(cc, control_rx);
            if !send_paths.is_empty() {
                app.send_paths(send_paths.clone());
            }
            Ok(Box::new(app))
        }),
    )
}

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

fn semibold_family() -> FontFamily {
    FontFamily::Name("semibold".into())
}

fn sb_font(size: f32) -> FontId {
    FontId::new(size, semibold_family())
}

fn semibold(text: impl Into<String>, size: f32) -> RichText {
    RichText::new(text.into())
        .size(size)
        .family(semibold_family())
}

fn install_fonts(ctx: &egui::Context) {
    // Segoe UI Variable (Windows 11) is the closest thing to San Francisco; fall back to Segoe UI.
    let mut fonts = egui::FontDefinitions::default();
    let regular = [
        "C:\\Windows\\Fonts\\SegUIVar.ttf",
        "C:\\Windows\\Fonts\\segoeui.ttf",
    ]
    .iter()
    .find_map(|p| std::fs::read(p).ok());
    let bold = [
        "C:\\Windows\\Fonts\\seguisb.ttf",
        "C:\\Windows\\Fonts\\segoeuib.ttf",
    ]
    .iter()
    .find_map(|p| std::fs::read(p).ok());

    if let Some(bytes) = regular {
        fonts
            .font_data
            .insert("system-ui".to_owned(), egui::FontData::from_owned(bytes));
        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .insert(0, "system-ui".to_owned());
    }
    let mut semi: Vec<String> = Vec::new();
    if let Some(bytes) = bold {
        fonts.font_data.insert(
            "system-ui-semibold".to_owned(),
            egui::FontData::from_owned(bytes),
        );
        semi.push("system-ui-semibold".to_owned());
    }
    // Always end with the regular proportional stack so every glyph still resolves.
    semi.extend(
        fonts
            .families
            .get(&FontFamily::Proportional)
            .cloned()
            .unwrap_or_default(),
    );
    fonts.families.insert(semibold_family(), semi);
    ctx.set_fonts(fonts);
}

fn apply_style(ctx: &egui::Context, p: Pal) {
    use egui::TextStyle;
    let mut style = (*ctx.style()).clone();
    style.text_styles = [
        (TextStyle::Heading, FontId::proportional(20.0)),
        (TextStyle::Body, FontId::proportional(13.0)),
        (TextStyle::Button, FontId::proportional(13.0)),
        (TextStyle::Small, FontId::proportional(11.0)),
        (TextStyle::Monospace, FontId::monospace(13.0)),
    ]
    .into();
    style.spacing.item_spacing = vec2(8.0, 10.0);
    style.spacing.button_padding = vec2(12.0, 5.0);
    style.spacing.interact_size.y = 26.0;
    style.spacing.scroll = egui::style::ScrollStyle::floating();

    style.visuals = if p.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    let v = &mut style.visuals;
    let r = Rounding::same(if p.m3 { 8.0 } else { 10.0 });
    v.panel_fill = Color32::TRANSPARENT;
    v.window_fill = p.panel;
    v.window_stroke = Stroke::new(1.0_f32, p.panel_edge);
    v.window_rounding = Rounding::same(if p.m3 { 28.0 } else { 18.0 });
    v.menu_rounding = Rounding::same(if p.m3 { 8.0 } else { 14.0 });
    v.extreme_bg_color = p.field;
    v.faint_bg_color = p.card;
    v.hyperlink_color = p.accent;
    v.selection.bg_fill = p.accent.gamma_multiply(0.45);
    v.selection.stroke = Stroke::new(1.0_f32, p.accent);
    v.override_text_color = None;

    let fg = Stroke::new(1.0_f32, p.text);
    for (w, fill) in [
        (&mut v.widgets.noninteractive, p.card),
        (&mut v.widgets.inactive, p.fill),
        (&mut v.widgets.hovered, p.fill_hover),
        (&mut v.widgets.active, p.fill_press),
        (&mut v.widgets.open, p.fill_hover),
    ] {
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
        w.bg_stroke = Stroke::NONE;
        w.fg_stroke = fg;
        w.rounding = r;
        w.expansion = 0.0;
    }
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, p.card_edge);
    if p.m3 {
        // Material 3 outlined text fields: 1px outline, 2px primary ring on focus.
        v.extreme_bg_color = Color32::TRANSPARENT;
        v.selection.bg_fill = p.accent.gamma_multiply(0.30);
        v.selection.stroke = Stroke::new(2.0_f32, p.accent);
        v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, p.text3);
        v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, p.text2);
        v.widgets.active.bg_stroke = Stroke::new(2.0_f32, p.accent);
        v.widgets.open.bg_stroke = Stroke::new(1.0_f32, p.text2);
        v.widgets.hovered.weak_bg_fill = p.fill_hover;
        v.widgets.active.weak_bg_fill = p.fill_press;
        v.popup_shadow = egui::epaint::Shadow {
            offset: vec2(0.0, 4.0),
            blur: 12.0,
            spread: 0.0,
            color: Color32::from_black_alpha(if p.dark { 110 } else { 40 }),
        };
    }

    ctx.set_style(style);
}

fn detect_dark(ctx: &egui::Context) -> bool {
    ctx.input(|i| i.raw.system_theme)
        .map(|t| t == egui::Theme::Dark)
        .unwrap_or(true)
}

// ---------------------------------------------------------------------------
// Small widgets
// ---------------------------------------------------------------------------

/// A rounded "grouped" container, like the inset lists in macOS System Settings.
fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let p = pal();
    let shadow = if p.m3 {
        egui::Shadow {
            offset: vec2(0.0, 3.0),
            blur: 10.0,
            spread: 0.0,
            color: Color32::from_black_alpha(if p.dark { 70 } else { 20 }),
        }
    } else {
        egui::Shadow::NONE
    };
    egui::Frame::none()
        .fill(p.card)
        .stroke(Stroke::new(1.0_f32, p.card_edge))
        .rounding(card_radius())
        .shadow(shadow)
        .inner_margin(egui::Margin::symmetric(if p.m3 { 18.0 } else { 14.0 }, if p.m3 { 16.0 } else { 12.0 }))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 8.0;
            add(ui)
        })
        .inner
}

fn hairline(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 0.0, pal().sep);
}

fn section_label(ui: &mut egui::Ui, text: &str) {
    ui.add_space(2.0);
    ui.label(semibold(text, 12.5).color(pal().text2));
}

fn status_dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(8.0, 8.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

fn status_pill(ui: &mut egui::Ui, text: &str) {
    let p = pal();
    let bad = text.contains("Could not") || text.contains("failed") || text.contains("rror");
    egui::Frame::none()
        .fill(p.fill)
        .rounding(if p.m3 { 12.0 } else { 16.0 })
        .inner_margin(egui::Margin::symmetric(12.0, if p.m3 { 10.0 } else { 7.0 }))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                status_dot(ui, if bad { p.red } else { p.accent });
                ui.label(RichText::new(shorten(text, 46)).size(12.0).color(p.text2));
            });
        });
}

/// Soft "raised surface" underlay for Material 3: a light glow up-left and a layered shadow
/// below. Draw it first, then fill the shape on top.
fn raised(painter: &egui::Painter, rect: Rect, radius: f32) {
    let p = pal();
    if !p.dark {
        painter.rect_filled(
            rect.expand(1.0).translate(vec2(-1.5, -1.5)),
            radius + 1.0,
            Color32::from_white_alpha(150),
        );
    }
    let a: u8 = if p.dark { 60 } else { 24 };
    for (dy, grow, alpha) in [(5.0_f32, 3.0_f32, a / 3), (3.0, 1.5, a / 2), (1.5, 0.0, a)] {
        painter.rect_filled(
            rect.expand(grow).translate(vec2(0.0, dy)),
            radius + grow,
            Color32::from_black_alpha(alpha),
        );
    }
}

/// Round glass button used in the header. Returns the rect so the caller can paint a glyph.
fn glass_circle(ui: &mut egui::Ui, active: bool) -> (Rect, egui::Response) {
    let p = pal();
    if p.m3 {
        let (rect, resp) = ui.allocate_exact_size(vec2(40.0, 40.0), egui::Sense::click());
        let fill = if active {
            p.tonal
        } else if resp.is_pointer_button_down_on() {
            p.fill_press
        } else if resp.hovered() {
            p.fill_hover
        } else {
            p.card
        };
        raised(ui.painter(), rect, 20.0);
        ui.painter().circle_filled(rect.center(), 20.0, fill);
        return (rect, resp.on_hover_cursor(egui::CursorIcon::PointingHand));
    }
    let (rect, resp) = ui.allocate_exact_size(vec2(34.0, 34.0), egui::Sense::click());
    let fill = if active {
        p.accent
    } else if resp.is_pointer_button_down_on() {
        p.fill_press
    } else if resp.hovered() {
        p.fill_hover
    } else {
        p.fill
    };
    ui.painter().circle_filled(rect.center(), 16.0, fill);
    ui.painter()
        .circle_stroke(rect.center(), 15.5, Stroke::new(1.0_f32, p.card_edge));
    (rect, resp.on_hover_cursor(egui::CursorIcon::PointingHand))
}

fn close_button(ui: &mut egui::Ui) -> egui::Response {
    let (rect, resp) = glass_circle(ui, false);
    let c = rect.center();
    let s = Stroke::new(2.0_f32, pal().text);
    ui.painter()
        .line_segment([c + vec2(-4.5, -4.5), c + vec2(4.5, 4.5)], s);
    ui.painter()
        .line_segment([c + vec2(4.5, -4.5), c + vec2(-4.5, 4.5)], s);
    resp.on_hover_text("Quit Shelf")
}

#[allow(dead_code)]
fn pin_button(ui: &mut egui::Ui, pinned: &mut bool) -> egui::Response {
    let (rect, resp) = glass_circle(ui, *pinned);
    if resp.clicked() {
        *pinned = !*pinned;
    }
    let c = rect.center();
    let col = if *pinned {
        pal().on_accent
    } else {
        pal().text2
    };
    let s = Stroke::new(1.5_f32, col);
    let p = ui.painter();
    p.rect_stroke(
        Rect::from_center_size(c + vec2(0.0, -3.5), vec2(6.0, 8.0)),
        2.0,
        s,
    );
    p.line_segment([c + vec2(-5.0, 1.5), c + vec2(5.0, 1.5)], s);
    p.line_segment([c + vec2(0.0, 1.5), c + vec2(0.0, 7.0)], s);
    resp.on_hover_text(if *pinned { "Pinned open" } else { "Keep open" })
}

#[allow(dead_code)]
/// Material 3 switch: 52x32 track, thumb grows and shows a check when on.
fn m3_switch(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let p = pal();
    let (rect, mut resp) = ui.allocate_exact_size(vec2(52.0, 32.0), egui::Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_responsive(resp.id, *on);
    let off_track = p.card.lerp_to_gamma(p.field, 0.6);
    let track = off_track.lerp_to_gamma(p.accent, t);
    let outline = p.text3.gamma_multiply(1.0 - t);
    let radius = egui::lerp(8.0..=12.0, t);
    let x = egui::lerp((rect.left() + 16.0)..=(rect.right() - 16.0), t);
    let center = Pos2::new(x, rect.center().y);
    let thumb = p.text3.lerp_to_gamma(p.on_accent, t);
    let painter = ui.painter();
    painter.rect_filled(rect, 16.0, track);
    if t < 0.99 {
        painter.rect_stroke(rect.shrink(1.0), 15.0, Stroke::new(2.0_f32, outline));
    }
    // Hover/press state layer around the thumb.
    if resp.is_pointer_button_down_on() || resp.hovered() {
        let layer = if on_state(t) { p.accent } else { p.text };
        painter.circle_filled(
            center,
            20.0,
            layer.gamma_multiply(if resp.is_pointer_button_down_on() {
                0.12
            } else {
                0.08
            }),
        );
    }
    painter.circle_filled(center, radius, thumb);
    if t > 0.5 {
        let s = Stroke::new(2.0_f32, p.accent.gamma_multiply(t));
        painter.line_segment([center + vec2(-4.5, 0.0), center + vec2(-1.5, 3.2)], s);
        painter.line_segment([center + vec2(-1.5, 3.2), center + vec2(4.5, -3.4)], s);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

#[allow(dead_code)]
fn on_state(t: f32) -> bool {
    t > 0.5
}

/// Switch in the current style (macOS or Material 3).
fn toggle(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    mac_switch(ui, on)
}

/// macOS-style switch.
fn mac_switch(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let p = pal();
    let (rect, mut resp) = ui.allocate_exact_size(vec2(52.0, 30.0), egui::Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_responsive(resp.id, *on);
    // Off: a frosted glass track. On: the bright accent blue.
    let off_track = if p.m3 {
        if p.dark {
            Color32::from_rgb(78, 85, 81)
        } else {
            Color32::from_rgb(201, 206, 203)
        }
    } else if p.dark {
        Color32::from_white_alpha(46)
    } else {
        Color32::from_rgba_unmultiplied(60, 90, 150, 56)
    };
    let track = off_track.lerp_to_gamma(p.accent, t);
    let x = egui::lerp((rect.left() + 15.0)..=(rect.right() - 15.0), t);
    let painter = ui.painter();
    painter.rect_filled(rect, 15.0, track);
    if !p.m3 {
        painter.rect_stroke(
            rect.shrink(0.5),
            14.5,
            Stroke::new(1.0_f32, Color32::from_white_alpha(if p.dark { 60 } else { 150 })),
        );
    }
    // Soft shadow, then the glossy white thumb.
    painter.circle_filled(
        Pos2::new(x, rect.center().y + 1.5),
        12.5,
        Color32::from_black_alpha(52),
    );
    painter.circle_filled(Pos2::new(x, rect.center().y), 12.5, Color32::WHITE);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn row_toggle(ui: &mut egui::Ui, label: &str, hint: &str, value: &mut bool) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.label(RichText::new(label).size(13.5).color(pal().text));
            ui.label(RichText::new(hint).size(11.5).color(pal().text2));
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            toggle(ui, value);
        });
    });
}

#[derive(Clone, Copy)]
enum Look {
    Primary,
    Glass,
    Tint(Color32),
    /// A choice in a group: a filter chip in Material 3, a capsule on macOS.
    Chip(bool),
}

/// Material 3 button or filter chip with hover/press state layers.
fn m3_button(
    ui: &mut egui::Ui,
    label: &str,
    look: Look,
    height: f32,
    width: Option<f32>,
    enabled: bool,
) -> egui::Response {
    let p = pal();
    let font = sb_font(if height >= 34.0 { 14.0 } else { 12.5 });
    let is_chip = matches!(look, Look::Chip(_));
    let selected = matches!(look, Look::Chip(true));
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font.clone(), Color32::WHITE);
    let check_w = if selected { 18.0 } else { 0.0 };
    let pad = if is_chip { 12.0 } else { height * 0.6 };
    let w = width.unwrap_or(galley.size().x + check_w + pad * 2.0);
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(vec2(w, height), sense);
    let down = enabled && resp.is_pointer_button_down_on();
    let hot = enabled && resp.hovered();
    let radius = rect.height() / 2.0;

    let (mut base, mut fg) = match look {
        Look::Primary => (p.accent, p.on_accent),
        Look::Glass => (p.card, p.text),
        Look::Tint(c) => (c.gamma_multiply(0.16), c),
        Look::Chip(true) => (p.tonal, p.text),
        Look::Chip(false) => (p.card, p.text),
    };
    if !enabled {
        base = p.text.gamma_multiply(0.12);
        fg = p.text.gamma_multiply(0.38);
    }
    let painter = ui.painter();
    if enabled && matches!(look, Look::Glass | Look::Chip(false)) {
        raised(painter, rect, radius);
    }
    painter.rect_filled(rect, radius, base);
    if down {
        painter.rect_filled(rect, radius, fg.gamma_multiply(0.12));
    } else if hot {
        painter.rect_filled(rect, radius, fg.gamma_multiply(0.08));
    }

    let content_w = check_w + galley.size().x;
    let start_x = rect.center().x - content_w / 2.0;
    let cy = rect.center().y;
    if selected {
        let c = Pos2::new(start_x + 7.0, cy);
        let s = Stroke::new(1.8_f32, fg);
        painter.line_segment([c + vec2(-4.0, 0.0), c + vec2(-1.2, 3.0)], s);
        painter.line_segment([c + vec2(-1.2, 3.0), c + vec2(4.5, -3.2)], s);
    }
    painter.text(
        Pos2::new(start_x + check_w, cy),
        Align2::LEFT_CENTER,
        label,
        font,
        fg,
    );
    if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// Capsule button. `width` of `None` hugs the label.
fn capsule(
    ui: &mut egui::Ui,
    label: &str,
    look: Look,
    height: f32,
    width: Option<f32>,
    enabled: bool,
) -> egui::Response {
    let p = pal();
    if p.m3 {
        return m3_button(ui, label, look, height, width, enabled);
    }
    let selected_chip = matches!(look, Look::Chip(true));
    let look = match look {
        Look::Chip(true) => Look::Primary,
        Look::Chip(false) => Look::Glass,
        other => other,
    };
    let font = sb_font(if height >= 34.0 { 14.0 } else { 12.5 });
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font.clone(), Color32::WHITE);
    // A selected chip shows a check mark in front of its label, like the mockup.
    let check_w = if selected_chip { 20.0 } else { 0.0 };
    let w = width.unwrap_or(galley.size().x + height * 0.95 + check_w);
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(vec2(w, height), sense);
    let down = enabled && resp.is_pointer_button_down_on();
    let hot = enabled && resp.hovered();
    let (mut fill, fg) = match look {
        Look::Primary => (p.accent, Color32::WHITE),
        Look::Glass => (
            if down {
                p.fill_press
            } else if hot {
                p.fill_hover
            } else {
                p.fill
            },
            p.text,
        ),
        Look::Tint(c) => (
            c.gamma_multiply(if down {
                0.34
            } else if hot {
                0.26
            } else {
                0.16
            }),
            c,
        ),
        Look::Chip(_) => (p.fill, p.text), // already mapped to Primary/Glass above
    };
    if let Look::Primary = look {
        if down {
            fill = fill.lerp_to_gamma(Color32::BLACK, 0.18);
        } else if hot {
            fill = fill.lerp_to_gamma(Color32::WHITE, 0.14);
        }
    }
    let (fill, fg) = if enabled {
        (fill, fg)
    } else {
        (fill.gamma_multiply(0.45), fg.gamma_multiply(0.55))
    };
    let radius = rect.height() / 2.0;
    ui.painter().rect_filled(rect, radius, fill);
    match look {
        Look::Primary => {
            // Light catching the top half of the blue pill.
            let sheen = Rect::from_min_max(rect.min, Pos2::new(rect.max.x, rect.center().y));
            ui.painter().rect_filled(
                sheen,
                Rounding {
                    nw: radius,
                    ne: radius,
                    sw: 0.0,
                    se: 0.0,
                },
                Color32::from_white_alpha(if enabled { 26 } else { 10 }),
            );
            ui.painter().rect_stroke(
                rect.shrink(0.5),
                radius,
                Stroke::new(1.0_f32, Color32::from_white_alpha(70)),
            );
        }
        Look::Glass => {
            ui.painter().rect_stroke(
                rect.shrink(0.5),
                radius,
                Stroke::new(1.0_f32, p.card_edge),
            );
        }
        _ => {}
    }
    if selected_chip {
        let cx = rect.center().x - galley.size().x / 2.0 - check_w / 2.0 + 7.0;
        let c = Pos2::new(cx, rect.center().y);
        let s = Stroke::new(2.0_f32, fg);
        ui.painter()
            .line_segment([c + vec2(-5.0, 0.5), c + vec2(-1.5, 4.0)], s);
        ui.painter()
            .line_segment([c + vec2(-1.5, 4.0), c + vec2(5.0, -3.5)], s);
        ui.painter().text(
            Pos2::new(cx + 12.0, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            font,
            fg,
        );
    } else {
        ui.painter()
            .text(rect.center(), Align2::CENTER_CENTER, label, font, fg);
    }
    if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

fn thin_progress(ui: &mut egui::Ui, frac: f32) {
    let p = pal();
    let h = if p.m3 { 7.0 } else { 5.0 };
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), h), egui::Sense::hover());
    let track = if p.m3 {
        p.fill_press
    } else {
        p.fill
    };
    ui.painter().rect_filled(rect, h / 2.0, track);
    let mut fill = rect;
    fill.set_width(rect.width() * frac.clamp(0.0, 1.0));
    ui.painter().rect_filled(fill, h / 2.0, p.accent);
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

/// Down arrow into a tray.
fn arrow_tray(painter: &egui::Painter, c: Pos2, color: Color32) {
    let s = Stroke::new(2.0_f32, color);
    painter.line_segment([c + vec2(0.0, -8.0), c + vec2(0.0, 4.0)], s);
    painter.line_segment([c + vec2(-5.0, -1.0), c + vec2(0.0, 4.0)], s);
    painter.line_segment([c + vec2(5.0, -1.0), c + vec2(0.0, 4.0)], s);
    painter.line_segment([c + vec2(-8.0, 9.0), c + vec2(8.0, 9.0)], s);
}

// ---- Glyphs for the macOS glass layout ------------------------------------------------------

type Glyph = fn(&egui::Painter, Pos2, Color32);

fn glyph_share(painter: &egui::Painter, c: Pos2, col: Color32) {
    let s = Stroke::new(1.9_f32, col);
    // Open box.
    painter.add(egui::Shape::line(
        vec![
            c + vec2(0.5, -8.0),
            c + vec2(-5.0, -8.0),
            c + vec2(-8.0, -5.0),
            c + vec2(-8.0, 5.0),
            c + vec2(-5.0, 8.0),
            c + vec2(5.0, 8.0),
            c + vec2(8.0, 5.0),
            c + vec2(8.0, 1.0),
        ],
        s,
    ));
    // Arrow leaving the box at the top right.
    painter.line_segment([c + vec2(-2.0, 2.0), c + vec2(8.0, -8.0)], s);
    painter.line_segment([c + vec2(3.0, -8.0), c + vec2(8.0, -8.0)], s);
    painter.line_segment([c + vec2(8.0, -8.0), c + vec2(8.0, -3.0)], s);
}

fn glyph_copy(painter: &egui::Painter, c: Pos2, col: Color32) {
    let s = Stroke::new(1.9_f32, col);
    // The part of the back sheet that peeks out from behind the front one.
    painter.add(egui::Shape::line(
        vec![
            c + vec2(3.5, -3.5),
            c + vec2(3.5, -8.5),
            c + vec2(-8.5, -8.5),
            c + vec2(-8.5, 3.5),
            c + vec2(-3.5, 3.5),
        ],
        s,
    ));
    painter.rect_stroke(
        Rect::from_center_size(c + vec2(2.5, 2.5), vec2(12.0, 12.0)),
        3.0,
        s,
    );
}

/// A QR code: three finder squares and a few data modules.
fn glyph_qr(painter: &egui::Painter, c: Pos2, col: Color32) {
    let s = Stroke::new(1.7_f32, col);
    for (x, y) in [(-8.5_f32, -8.5_f32), (1.5, -8.5), (-8.5, 1.5)] {
        painter.rect_stroke(Rect::from_min_size(c + vec2(x, y), vec2(7.0, 7.0)), 1.5, s);
        painter.rect_filled(
            Rect::from_center_size(c + vec2(x + 3.5, y + 3.5), vec2(2.4, 2.4)),
            0.0,
            col,
        );
    }
    for (x, y) in [(2.0_f32, 2.0_f32), (6.2, 2.0), (2.0, 6.2), (6.2, 6.2)] {
        painter.rect_filled(Rect::from_min_size(c + vec2(x, y), vec2(2.3, 2.3)), 0.0, col);
    }
}

/// Lucide "airplay": a screen with an open bottom and a triangle beneath it.
fn glyph_airplay(painter: &egui::Painter, c: Pos2, col: Color32) {
    let k = 0.92_f32;
    let s = Stroke::new(1.9_f32, col);
    let pt = |x: f32, y: f32| c + vec2(x - 12.0, y - 12.0) * k;
    let mut pts = vec![pt(5.0, 17.0), pt(4.0, 17.0)];
    // Four rounded corners of radius 2, walked clockwise from the bottom left.
    for (cx, cy, a0) in [
        (4.0_f32, 15.0_f32, 90.0_f32),
        (4.0, 5.0, 180.0),
        (20.0, 5.0, 270.0),
        (20.0, 15.0, 0.0),
    ] {
        for i in 0..=6 {
            let a = (a0 + 90.0 * i as f32 / 6.0).to_radians();
            pts.push(pt(cx + 2.0 * a.cos(), cy + 2.0 * a.sin()));
        }
    }
    pts.push(pt(19.0, 17.0));
    painter.add(egui::Shape::line(pts, s));
    painter.add(egui::Shape::closed_line(
        vec![pt(12.0, 15.0), pt(17.0, 21.0), pt(7.0, 21.0)],
        s,
    ));
}

fn glyph_gear(painter: &egui::Painter, c: Pos2, col: Color32) {
    let s = Stroke::new(1.9_f32, col);
    let teeth = Stroke::new(3.0_f32, col);
    for i in 0..8 {
        let a = (i as f32 * 45.0).to_radians();
        let d = vec2(a.cos(), a.sin());
        painter.line_segment([c + d * 7.5, c + d * 10.0], teeth);
    }
    painter.circle_stroke(c, 7.0, s);
    painter.circle_stroke(c, 3.0, s);
}

fn glyph_pause(painter: &egui::Painter, c: Pos2, col: Color32) {
    let s = Stroke::new(1.9_f32, col);
    painter.rect_stroke(Rect::from_center_size(c + vec2(-5.5, 0.0), vec2(5.5, 17.0)), 2.5, s);
    painter.rect_stroke(Rect::from_center_size(c + vec2(5.5, 0.0), vec2(5.5, 17.0)), 2.5, s);
}

fn glyph_pin(painter: &egui::Painter, c: Pos2, col: Color32) {
    let s = Stroke::new(1.9_f32, col);
    painter.rect_stroke(Rect::from_center_size(c + vec2(0.0, -4.5), vec2(7.0, 10.0)), 2.5, s);
    painter.line_segment([c + vec2(-6.0, 1.0), c + vec2(6.0, 1.0)], s);
    painter.line_segment([c + vec2(0.0, 1.0), c + vec2(0.0, 9.0)], s);
}

fn glyph_doc(painter: &egui::Painter, c: Pos2, col: Color32) {
    let s = Stroke::new(1.9_f32, col);
    painter.rect_stroke(Rect::from_center_size(c, vec2(15.0, 19.0)), 3.5, s);
    painter.line_segment([c + vec2(-3.5, -3.5), c + vec2(3.5, -3.5)], s);
    painter.line_segment([c + vec2(-3.5, 0.5), c + vec2(3.5, 0.5)], s);
    painter.line_segment([c + vec2(-3.5, 4.5), c + vec2(1.0, 4.5)], s);
}

fn glyph_swap(painter: &egui::Painter, c: Pos2, col: Color32) {
    let s = Stroke::new(1.9_f32, col);
    // Right-pointing arrow on top, left-pointing arrow below.
    painter.line_segment([c + vec2(-9.0, -4.5), c + vec2(9.0, -4.5)], s);
    painter.line_segment([c + vec2(4.5, -9.0), c + vec2(9.0, -4.5)], s);
    painter.line_segment([c + vec2(4.5, 0.0), c + vec2(9.0, -4.5)], s);
    painter.line_segment([c + vec2(9.0, 4.5), c + vec2(-9.0, 4.5)], s);
    painter.line_segment([c + vec2(-4.5, 0.0), c + vec2(-9.0, 4.5)], s);
    painter.line_segment([c + vec2(-4.5, 9.0), c + vec2(-9.0, 4.5)], s);
}

fn glyph_sliders(painter: &egui::Painter, c: Pos2, col: Color32) {
    let s = Stroke::new(1.9_f32, col);
    painter.line_segment([c + vec2(-9.0, -5.0), c + vec2(9.0, -5.0)], s);
    painter.line_segment([c + vec2(-9.0, 5.0), c + vec2(9.0, 5.0)], s);
    painter.circle_filled(c + vec2(3.0, -5.0), 3.6, pal().card);
    painter.circle_stroke(c + vec2(3.0, -5.0), 3.6, s);
    painter.circle_filled(c + vec2(-3.0, 5.0), 3.6, pal().card);
    painter.circle_stroke(c + vec2(-3.0, 5.0), 3.6, s);
}

fn glyph_folder(painter: &egui::Painter, c: Pos2, col: Color32) {
    let s = Stroke::new(1.9_f32, col);
    painter.add(egui::Shape::line(
        vec![
            c + vec2(-9.5, -4.0),
            c + vec2(-9.5, -7.0),
            c + vec2(-3.0, -7.0),
            c + vec2(-0.5, -4.0),
            c + vec2(9.5, -4.0),
            c + vec2(9.5, 7.5),
            c + vec2(-9.5, 7.5),
            c + vec2(-9.5, -4.0),
        ],
        s,
    ));
}

/// Dark, see-through disc that holds a glyph (the round icon wells in the mockup).
fn badge_color() -> Color32 {
    pal().well
}

/// Large glass pill with an icon well on the left and a two-line label, used for the drop
/// target and the Clipboard / Devices tiles. `highlight` (0..=1) tints it with the accent.
fn glass_tile(
    ui: &mut egui::Ui,
    width: f32,
    height: f32,
    glyph: Glyph,
    title: &str,
    subtitle: &str,
    highlight: f32,
    interactive: bool,
    tonal: bool,
) -> egui::Response {
    let p = pal();
    let sense = if interactive {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(vec2(width, height), sense);
    let base = if interactive && resp.is_pointer_button_down_on() {
        p.fill_press
    } else if interactive && resp.hovered() {
        p.fill_hover
    } else {
        p.card
    };
    let glow = if p.m3 { p.tonal } else { p.accent.gamma_multiply(0.55) };
    let fill = base.lerp_to_gamma(glow, highlight);
    let radius = height / 2.0;
    if p.m3 {
        // Raised, soft-shadowed surface.
        raised(ui.painter(), rect, radius);
        ui.painter().rect_filled(rect, radius, fill);
    } else {
        let edge = p.card_edge.lerp_to_gamma(p.accent, highlight);
        ui.painter().rect_filled(rect, radius, fill);
        // Light catching the top half of the glass.
        ui.painter().rect_filled(
            Rect::from_min_max(rect.min, Pos2::new(rect.max.x, rect.center().y)),
            Rounding {
                nw: radius,
                ne: radius,
                sw: 0.0,
                se: 0.0,
            },
            Color32::from_white_alpha(if p.dark { 10 } else { 40 }),
        );
        ui.painter()
            .rect_stroke(rect.shrink(0.5), radius, Stroke::new(1.0_f32, edge));
    }

    let painter = ui.painter().with_clip_rect(rect);
    let disc = height * 0.62;
    let c = Pos2::new(rect.left() + (height - disc) / 2.0 + disc / 2.0, rect.center().y);
    let well = if p.m3 && tonal { p.tonal } else { p.well };
    painter.circle_filled(c, disc / 2.0, well);
    glyph(&painter, c, p.text);

    let tx = c.x + disc / 2.0 + 12.0;
    let size = if width > 260.0 { 17.0 } else { 14.5 };
    let title_font = if p.m3 {
        FontId::proportional(size + 1.0)
    } else {
        sb_font(size)
    };
    painter.text(
        Pos2::new(tx, rect.center().y - 9.0),
        Align2::LEFT_CENTER,
        title,
        title_font,
        p.text,
    );
    painter.text(
        Pos2::new(tx, rect.center().y + 10.0),
        Align2::LEFT_CENTER,
        subtitle,
        FontId::proportional(if width > 260.0 { 13.0 } else { 12.0 }),
        p.text2,
    );
    if interactive {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// Raised pill with a glyph and a label (Material 3 "All settings" / "Choose folder").
fn icon_pill(ui: &mut egui::Ui, glyph: Glyph, label: &str, height: f32) -> egui::Response {
    let p = pal();
    let font = FontId::proportional(14.5);
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font.clone(), p.text);
    let w = galley.size().x + 66.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(w, height), egui::Sense::click());
    let fill = if resp.is_pointer_button_down_on() {
        p.fill_press
    } else if resp.hovered() {
        p.fill_hover
    } else {
        p.card
    };
    let radius = height / 2.0;
    raised(ui.painter(), rect, radius);
    ui.painter().rect_filled(rect, radius, fill);
    let ic = Pos2::new(rect.left() + 30.0, rect.center().y);
    glyph(ui.painter(), ic, p.text);
    ui.painter().text(
        Pos2::new(ic.x + 20.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        font,
        p.text,
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Big round quick-action button: a glass ring around a darker well with a glyph in it.
fn quick_circle(ui: &mut egui::Ui, size: f32, glyph: Glyph, active: bool) -> egui::Response {
    let p = pal();
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), egui::Sense::click());
    let outer = if resp.is_pointer_button_down_on() {
        p.fill_press
    } else if resp.hovered() {
        p.fill_hover
    } else {
        p.card
    };
    let c = rect.center();
    if p.m3 {
        let fill = if active { p.tonal } else { outer };
        raised(ui.painter(), rect, size / 2.0);
        ui.painter().circle_filled(c, size / 2.0, fill);
        glyph(ui.painter(), c, p.text);
        return resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    }
    let painter = ui.painter();
    painter.circle_filled(c, size / 2.0, outer);
    painter.circle_stroke(c, size / 2.0 - 0.5, Stroke::new(1.0_f32, p.card_edge));
    let well = size * 0.31;
    painter.circle_filled(c, well, if active { p.accent } else { badge_color() });
    glyph(painter, c, if active { p.on_accent } else { p.text });
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Rows for the active (or failed) transfers, with progress and a cancel button.
fn transfer_cards(ui: &mut egui::Ui, transfers: &[TransferStatus], actions: &mut Vec<Action>) {
    let p = pal();
    for transfer in transfers
        .iter()
        .rev()
        .filter(|t| !t.finished || t.error.is_some())
        .take(3)
    {
        let elapsed = transfer.started.elapsed().as_secs_f64().max(0.001);
        let speed = transfer.done as f64 / elapsed;
        let remaining = transfer.total.saturating_sub(transfer.done) as f64;
        let fraction = if transfer.total == 0 {
            1.0
        } else {
            transfer.done as f32 / transfer.total as f32
        };
        let verb = match transfer.direction {
            TransferDirection::Upload => "Uploading",
            TransferDirection::Download => "Downloading",
        };
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!(
                        "{verb} {} · {}",
                        shorten(&transfer.name, 24),
                        shorten(&transfer.peer, 18)
                    ))
                    .size(11.5)
                    .color(p.text),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("{}%", (fraction * 100.0) as u8))
                            .size(11.5)
                            .color(p.text2),
                    );
                });
            });
            if let Some(error) = &transfer.error {
                ui.label(
                    RichText::new(format!("Failed: {}", shorten(error, 52)))
                        .size(11.5)
                        .color(p.red),
                );
            } else {
                ui.label(
                    RichText::new(format!(
                        "{} · {}",
                        transfer_rate(speed),
                        transfer_eta(remaining / speed)
                    ))
                    .size(11.5)
                    .color(p.text2),
                );
            }
            thin_progress(ui, fraction);
            if !transfer.finished
                && capsule(ui, tr("Cancel"), Look::Tint(p.red), 24.0, None, true).clicked()
            {
                actions.push(Action::CancelTransfer {
                    direction: transfer.direction,
                    name: transfer.name.clone(),
                    peer: transfer.peer.clone(),
                });
            }
        });
    }
}

/// "Transfers · N active" card with an overall progress track (macOS glass layout).
fn transfers_summary(ui: &mut egui::Ui, transfers: &[TransferStatus]) {
    let active: Vec<&TransferStatus> = transfers
        .iter()
        .filter(|t| !t.finished && t.error.is_none())
        .collect();
    let (done, total) = active
        .iter()
        .fold((0u64, 0u64), |(d, t), x| (d + x.done, t + x.total));
    let frac = if total == 0 { 0.0 } else { done as f32 / total as f32 };
    let p = pal();
    let title = format!("{} · {} {}", tr("Transfers"), active.len(), tr("active"));
    card(ui, |ui| {
        if p.m3 {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 16.0;
                let (r, _) = ui.allocate_exact_size(vec2(30.0, 30.0), egui::Sense::hover());
                glyph_swap(ui.painter(), r.center(), p.text);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 8.0;
                    ui.label(RichText::new(title.clone()).size(16.0).color(p.text));
                    thin_progress(ui, frac);
                });
            });
        } else {
            ui.label(semibold(title.clone(), 15.0).color(p.text));
            ui.add_space(2.0);
            thin_progress(ui, frac);
        }
    });
}

/// The drop target: a wide pill that lights up while files hover over it.
fn drop_zone(ui: &mut egui::Ui, hovering: bool) {
    let t = ui
        .ctx()
        .animate_bool_responsive(egui::Id::new("drop_zone_hover"), hovering);
    let title = if hovering { "Release to send" } else { tr("Drop files") };
    glass_tile(
        ui,
        ui.available_width(),
        84.0,
        glyph_share,
        title,
        tr("Drag here to send"),
        t,
        false,
        true,
    );
}

fn draw_kind_icon(painter: &egui::Painter, rect: Rect, kind: &ItemKind) {
    let p = pal();
    let color = match kind {
        ItemKind::File => p.accent,
        ItemKind::Text => p.orange,
        ItemKind::Image => p.purple,
    };
    // Material 3: flat tonal tile with a dark glyph. macOS: glossy tile with a white glyph.
    let glyph = if p.m3 {
        let tile = if p.dark {
            color.gamma_multiply(0.28)
        } else {
            color.gamma_multiply(0.18)
        };
        painter.rect_filled(rect, 14.0, tile);
        color
    } else {
        painter.rect_filled(rect, 12.0, color);
        // Soft top sheen so the tile reads as glass rather than flat paint.
        let sheen = Rect::from_min_max(rect.min, Pos2::new(rect.max.x, rect.center().y));
        painter.rect_filled(
            sheen,
            Rounding {
                nw: 12.0,
                ne: 12.0,
                sw: 0.0,
                se: 0.0,
            },
            Color32::from_white_alpha(22),
        );
        Color32::WHITE
    };
    let c = rect.center();
    let s = Stroke::new(1.8_f32, glyph);
    match kind {
        ItemKind::File => {
            painter.rect_stroke(Rect::from_center_size(c, vec2(13.0, 17.0)), 3.0, s);
            painter.line_segment([c + vec2(-3.0, -2.0), c + vec2(3.0, -2.0)], s);
            painter.line_segment([c + vec2(-3.0, 2.5), c + vec2(3.0, 2.5)], s);
        }
        ItemKind::Text => {
            painter.line_segment([c + vec2(-8.0, -6.0), c + vec2(8.0, -6.0)], s);
            painter.line_segment([c + vec2(-8.0, 0.0), c + vec2(8.0, 0.0)], s);
            painter.line_segment([c + vec2(-8.0, 6.0), c + vec2(2.0, 6.0)], s);
        }
        ItemKind::Image => {
            painter.rect_stroke(Rect::from_center_size(c, vec2(20.0, 15.0)), 3.5, s);
            painter.circle_filled(c + vec2(-4.0, -2.0), 1.9, glyph);
            painter.add(egui::Shape::line(
                vec![
                    c + vec2(-8.0, 5.5),
                    c + vec2(-2.0, 0.5),
                    c + vec2(1.5, 3.5),
                    c + vec2(4.5, 0.5),
                    c + vec2(8.0, 5.0),
                ],
                s,
            ));
        }
    }
}

/// Round avatar with a little monitor and an online/offline badge.
fn draw_device_icon(painter: &egui::Painter, rect: Rect, online: bool) {
    let p = pal();
    let c = rect.center();
    painter.circle_filled(c, rect.width() / 2.0, p.fill);
    let col = if online { p.text } else { p.text3 };
    let s = Stroke::new(1.5_f32, col);
    painter.rect_stroke(
        Rect::from_center_size(c + vec2(0.0, -1.5), vec2(15.0, 10.0)),
        2.5,
        s,
    );
    painter.line_segment([c + vec2(-4.0, 7.0), c + vec2(4.0, 7.0)], s);
    let badge = c + vec2(9.5, 9.5);
    let ring = if p.m3 {
        p.card
    } else if p.dark {
        Color32::from_rgb(40, 40, 44)
    } else {
        Color32::from_rgb(244, 244, 247)
    };
    painter.circle_filled(badge, 5.5, ring);
    painter.circle_filled(badge, 3.8, if online { p.good } else { p.text3 });
}

/// The six pairing digits as separate glass tiles.
fn code_tiles(ui: &mut egui::Ui, code: &str) {
    let p = pal();
    let gap = 6.0;
    let tile_w = ((ui.available_width() - gap * 5.0) / 6.0).clamp(30.0, 46.0);
    let total = tile_w * 6.0 + gap * 5.0;
    let (rect, _) = ui.allocate_exact_size(vec2(total, 56.0), egui::Sense::hover());
    let digits: Vec<char> = code.chars().take(6).collect();
    for i in 0..6 {
        let x = rect.left() + i as f32 * (tile_w + gap);
        let r = Rect::from_min_size(Pos2::new(x, rect.top()), vec2(tile_w, rect.height()));
        ui.painter()
            .rect_filled(r, if p.m3 { 12.0 } else { 14.0 }, p.fill);
        if p.m3 {
            ui.painter()
                .rect_stroke(r, 12.0, Stroke::new(1.0_f32, p.text3));
        }
        if let Some(d) = digits.get(i) {
            ui.painter().text(
                r.center(),
                Align2::CENTER_CENTER,
                d.to_string(),
                sb_font(28.0),
                p.text,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

struct ShelfItem {
    item: ReceivedItem,
    received: Instant,
    preview: Option<egui::TextureHandle>,
}

struct PairDialog {
    device: NearbyDevice,
    code: String,
    busy: bool,
    error: Option<String>,
}

/// The phone-upload page that is currently open, shown as a QR code.
struct PhoneUpload {
    url: String,
    until: Instant,
}

struct TransferStatus {
    name: String,
    peer: String,
    direction: TransferDirection,
    done: u64,
    total: u64,
    started: Instant,
    finished: bool,
    error: Option<String>,
}

struct Receiving {
    name: String,
    peer: String,
    frac: Option<f32>,
    speed: f64,
}

/// Material 3 "notch": an inverse-surface pill with a circular progress indicator that
/// grows in at the top of the panel while this device is receiving a file.
fn m3_receive_notch(ui: &mut egui::Ui, receiving: Option<&Receiving>) {
    let p = pal();
    let open = ui
        .ctx()
        .animate_bool_responsive(egui::Id::new("m3_receive_notch"), receiving.is_some());
    let Some(r) = receiving else { return };
    if open <= 0.01 {
        return;
    }
    const H: f32 = 56.0;
    let (rect, _) =
        ui.allocate_exact_size(vec2(ui.available_width(), H * open), egui::Sense::hover());
    let full = Rect::from_min_size(rect.min, vec2(rect.width(), H));
    let painter = ui.painter().with_clip_rect(rect);
    let (bg, fg, arc) = if p.dark {
        (
            Color32::from_rgb(230, 225, 229),
            Color32::from_rgb(49, 48, 51),
            p.accent,
        )
    } else {
        (
            Color32::from_rgb(49, 48, 51),
            Color32::from_rgb(244, 239, 244),
            p.accent.lerp_to_gamma(Color32::WHITE, 0.45),
        )
    };
    painter.rect_filled(full, 28.0, bg);

    // Circular progress indicator.
    let c = Pos2::new(full.left() + 30.0, full.center().y);
    let radius = 13.0;
    painter.circle_stroke(c, radius, Stroke::new(3.0_f32, fg.gamma_multiply(0.22)));
    let t = ui.input(|i| i.time) as f32;
    let (start, sweep) = match r.frac {
        Some(f) => (
            -std::f32::consts::FRAC_PI_2,
            f.clamp(0.0, 1.0) * std::f32::consts::TAU,
        ),
        None => (t * 4.0, 1.7 + 0.8 * (t * 2.5).sin()),
    };
    let steps = ((sweep / std::f32::consts::TAU) * 64.0).ceil().max(2.0) as usize;
    let pts: Vec<Pos2> = (0..=steps)
        .map(|i| {
            let a = start + sweep * i as f32 / steps as f32;
            c + vec2(a.cos(), a.sin()) * radius
        })
        .collect();
    painter.add(egui::Shape::line(pts, Stroke::new(3.0_f32, arc)));
    let tip = Stroke::new(1.6_f32, fg);
    painter.line_segment([c + vec2(0.0, -4.0), c + vec2(0.0, 3.5)], tip);
    painter.line_segment([c + vec2(-3.0, 0.5), c + vec2(0.0, 3.5)], tip);
    painter.line_segment([c + vec2(3.0, 0.5), c + vec2(0.0, 3.5)], tip);

    let x = full.left() + 54.0;
    painter.text(
        Pos2::new(x, full.center().y - 9.0),
        Align2::LEFT_CENTER,
        format!("Receiving {}", shorten(&r.name, 22)),
        sb_font(13.0),
        fg,
    );
    let detail = if r.speed > 0.0 {
        format!(
            "{}/s · from {}",
            human_size(r.speed as u64),
            shorten(&r.peer, 16)
        )
    } else {
        format!("from {}", shorten(&r.peer, 16))
    };
    painter.text(
        Pos2::new(x, full.center().y + 9.0),
        Align2::LEFT_CENTER,
        detail,
        FontId::proportional(11.5),
        fg.gamma_multiply(0.75),
    );
    if let Some(f) = r.frac {
        painter.text(
            Pos2::new(full.right() - 18.0, full.center().y),
            Align2::RIGHT_CENTER,
            format!("{}%", (f.clamp(0.0, 1.0) * 100.0).round() as u32),
            sb_font(15.0),
            fg,
        );
    }
}

fn notify_arrival(item: &ReceivedItem) {
    let kind = match item.kind {
        ItemKind::File => "File",
        ItemKind::Image => "Image",
        ItemKind::Text => "Clipboard text",
    };
    let body = format!("{} from {}", item.name, item.from);
    let _ = notify_rust::Notification::new()
        .summary(&format!("Shelf received {kind}"))
        .body(&body)
        .appname("Shelf")
        .show();
}

fn notify_room_message(title: &str, body: &str) {
    // Show on a worker thread: on Windows the toast call can block the UI for a moment.
    let (title, body) = (title.to_owned(), body.to_owned());
    std::thread::spawn(move || {
        let _ = notify_rust::Notification::new()
            .summary(&title)
            .body(&body)
            .appname("Shelf")
            .show();
    });
}

/// A file request from a room member, kept until the owner handles or dismisses it.
struct IncomingRequest {
    room_id: String,
    room_name: String,
    name: String,
    from: String,
}

enum Action {
    SendClipboard,
    /// Opens or closes the QR code a phone can scan to send files to this shelf.
    TogglePhoneUpload,
    StartPairing,
    CancelPairing,
    OpenPair(NearbyDevice),
    ConnectPair,
    ClosePair,
    Unpair(String),
    Copy(usize),
    Reveal(usize),
    Remove(usize),
    ClearAll,
    CancelTransfer {
        direction: TransferDirection,
        name: String,
        peer: String,
    },
    CreateRoom,
    LeaveRoom(String),
    FulfillRequest(usize),
    DismissRequest(usize),
    JoinRoom(SocketAddr),
    DeleteRoomFile {
        room_id: String,
        hash: String,
    },
    DeleteRoom(String),
    RequestRoomFile {
        room_id: String,
    },
    SubmitRoomFile {
        room_id: String,
    },
    CreateAssignment {
        room_id: String,
    },
    ChooseAssignmentFiles,
    UpdateRoom {
        room_id: String,
        update: RoomUpdate,
    },
    ChooseReceiveFolder,
    SaveDeviceGroup,
    UseDeviceGroup(DeviceGroup),
    ExportRoom(String),
    ImportRoom,
}

struct ShelfApp {
    ctx: egui::Context,
    handle: Handle,
    rx: Receiver<Event>,
    clip_tx: Sender<Result<Payload, String>>,
    clip_rx: Receiver<Result<Payload, String>>,
    control_rx: Receiver<Vec<PathBuf>>,

    device_name: String,
    device_public_key: String,
    paired: Vec<PeerStatus>,
    device_groups: Vec<DeviceGroup>,
    receive_dir: Option<PathBuf>,
    nearby: Vec<NearbyDevice>,
    items: Vec<ShelfItem>,
    target: Target,
    status: String,
    transfers: Vec<TransferStatus>,
    audit: Vec<String>,
    rooms: Vec<Room>,
    room_name_input: String,
    active_room_id: Option<String>,
    room_id_input: String,
    room_code_input: String,
    room_request_input: String,
    room_assignment_input: String,
    room_announcement_input: String,
    room_clipboard_input: String,
    room_note_title_input: String,
    room_note_body_input: String,
    room_link_title_input: String,
    room_link_url_input: String,
    room_expiration_input: String,
    room_requests: Vec<IncomingRequest>,
    group_name_input: String,
    assignment_title_input: String,
    assignment_deadline_input: String,
    assignment_instructions_input: String,
    assignment_files: Vec<PathBuf>,
    my_code: Option<(String, Instant)>,
    phone_upload: Option<PhoneUpload>,
    pair_dialog: Option<PairDialog>,

    auto_copy: bool,
    receiving_paused: bool,
    pinned: bool,
    side: Side,
    monitor: usize,
    material: bool,
    saved_settings: Settings,
    /// macOS glass layout: show rooms, devices, activity and the advanced settings.
    show_all_settings: bool,
    /// Set when the Devices tile is clicked so the Devices section opens.
    devices_focus: bool,

    monitors: Vec<monitors::Monitor>,
    monitors_checked: Instant,
    _tray: Option<TrayIcon>,
    _hotkeys: Option<GlobalHotKeyManager>,
    hotkey_toggle_id: u32,
    hotkey_send_clipboard_id: u32,
    hotkey_closed: bool,

    styled: Option<(bool, bool)>,
    logo: Option<egui::TextureHandle>,

    cur_w: f32,
    last_geo: Option<(Vec2, Pos2)>,
    last_active: Instant,
    open_until: Instant,
}

fn audit_clock() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() % 86_400)
        .unwrap_or(0);
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3_600,
        (seconds % 3_600) / 60,
        seconds % 60
    )
}

fn room_qr(ui: &mut egui::Ui, room: &Room) {
    let payload = format!("shelf://room/{}/{}", room.id, room.invite_code);
    draw_qr(ui, &payload, 132.0, 0);
}

/// Draws `payload` as a black-on-white QR code `side` points wide. `quiet` is the blank
/// border in modules; phone cameras need about 2 to read a code reliably.
fn draw_qr(ui: &mut egui::Ui, payload: &str, side: f32, quiet: usize) {
    let Ok(code) = qrcode::QrCode::new(payload.as_bytes()) else {
        return;
    };
    let modules = code.width();
    let (rect, _) = ui.allocate_exact_size(vec2(side, side), egui::Sense::hover());
    ui.painter().rect_filled(rect, 8.0, Color32::WHITE);
    let colors = code.to_colors();
    let cell = side / (modules + 2 * quiet) as f32;
    let origin = rect.min + vec2(quiet as f32 * cell, quiet as f32 * cell);
    for y in 0..modules {
        for x in 0..modules {
            if colors[y * modules + x] == qrcode::Color::Dark {
                let min = origin + vec2(x as f32 * cell, y as f32 * cell);
                ui.painter().rect_filled(
                    Rect::from_min_size(min, vec2(cell + 0.5, cell + 0.5)),
                    0.0,
                    Color32::BLACK,
                );
            }
        }
    }
}

fn parse_room_invite(value: &str) -> Option<(String, String)> {
    let rest = value.trim().strip_prefix("shelf://room/")?;
    let (room_id, invite_code) = rest.split_once('/')?;
    if room_id.is_empty() || invite_code.is_empty() {
        return None;
    }
    Some((room_id.to_string(), invite_code.to_string()))
}

impl ShelfApp {
    fn new(cc: &eframe::CreationContext<'_>, control_rx: Receiver<Vec<PathBuf>>) -> Self {
        let ctx = cc.egui_ctx.clone();
        let wake_ctx = ctx.clone();
        let (handle, rx) =
            shelf_core::start(move || wake_ctx.request_repaint()).expect("failed to start Shelf");
        let (clip_tx, clip_rx) = channel();
        let logo = load_logo(&ctx);
        sweep_inbox();
        install_fonts(&ctx);
        let settings = Settings::load();
        let tray = build_tray(&ctx);
        let (hotkeys, hotkey_toggle_id, hotkey_send_clipboard_id) = match register_hotkeys() {
            Some((manager, toggle, send_clipboard)) => (Some(manager), toggle, send_clipboard),
            None => (None, 0, 0),
        };
        let now = Instant::now();
        Self {
            ctx,
            handle,
            rx,
            clip_tx,
            clip_rx,
            control_rx,
            device_name: String::new(),
            device_public_key: String::new(),
            paired: Vec::new(),
            device_groups: Vec::new(),
            receive_dir: None,
            nearby: Vec::new(),
            items: Vec::new(),
            target: Target::All,
            status: String::new(),
            transfers: Vec::new(),
            audit: Vec::new(),
            rooms: Vec::new(),
            room_name_input: String::new(),
            active_room_id: None,
            room_id_input: String::new(),
            room_code_input: String::new(),
            room_request_input: String::new(),
            room_assignment_input: String::new(),
            room_announcement_input: String::new(),
            room_clipboard_input: String::new(),
            room_note_title_input: String::new(),
            room_note_body_input: String::new(),
            room_link_title_input: String::new(),
            room_link_url_input: String::new(),
            room_expiration_input: String::new(),
            room_requests: Vec::new(),
            group_name_input: String::new(),
            assignment_title_input: String::new(),
            assignment_deadline_input: String::new(),
            assignment_instructions_input: String::new(),
            assignment_files: Vec::new(),
            my_code: None,
            phone_upload: None,
            pair_dialog: None,
            auto_copy: settings.auto_copy,
            receiving_paused: false,
            pinned: settings.pinned,
            side: settings.side,
            monitor: settings.monitor,
            material: settings.material,
            saved_settings: settings,
            show_all_settings: false,
            devices_focus: false,
            monitors: monitors::list(),
            monitors_checked: now,
            _tray: tray,
            _hotkeys: hotkeys,
            hotkey_toggle_id,
            hotkey_send_clipboard_id,
            hotkey_closed: false,
            styled: None,
            logo,
            cur_w: COLLAPSED_W,
            last_geo: None,
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

    fn record_audit(&mut self, message: impl Into<String>) {
        self.audit
            .push(format!("[{}] {}", audit_clock(), message.into()));
        if self.audit.len() > 100 {
            self.audit.remove(0);
        }
    }

    fn update_transfer(
        &mut self,
        direction: TransferDirection,
        name: String,
        peer: String,
        done: u64,
        total: u64,
    ) {
        let index = self.transfers.iter().position(|t| {
            t.direction == direction && t.name == name && t.peer == peer && !t.finished
        });
        if let Some(index) = index {
            let transfer = &mut self.transfers[index];
            transfer.done = done;
            transfer.total = total;
        } else {
            self.transfers.push(TransferStatus {
                name,
                peer,
                direction,
                done,
                total,
                started: Instant::now(),
                finished: false,
                error: None,
            });
            if self.transfers.len() > 8 {
                self.transfers.remove(0);
            }
        }
    }

    fn finish_download(&mut self, name: &str, peer: &str) {
        if let Some(transfer) = self.transfers.iter_mut().rev().find(|t| {
            t.direction == TransferDirection::Download
                && t.name == name
                && t.peer == peer
                && !t.finished
        }) {
            transfer.done = transfer.total;
            transfer.finished = true;
        }
    }

    fn fail_latest_transfer(&mut self, error: String) {
        if let Some(transfer) = self.transfers.iter_mut().rev().find(|t| !t.finished) {
            transfer.finished = true;
            transfer.error = Some(error);
        }
    }

    fn drain_events(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Event::RoomUpdated(room) => {
                    let was_known = self.rooms.iter().any(|existing| existing.id == room.id);
                    let previous = self
                        .rooms
                        .iter()
                        .find(|existing| existing.id == room.id)
                        .cloned();
                    if let Some(existing) = self.rooms.iter_mut().find(|r| r.id == room.id) {
                        *existing = room.clone();
                    } else {
                        self.rooms.push(room.clone());
                    }
                    if !was_known && self.active_room_id.is_none() {
                        self.active_room_id = Some(room.id.clone());
                        self.set_status(format!("Room active: {}", room.name));
                    }
                    self.record_audit(format!("Room updated: {}", room.name));
                    if was_known {
                        self.set_status(format!("Room ready: {}", room.name));
                        if let Some(old) = previous {
                            for announcement in room.announcements.iter().filter(|item| {
                                !old.announcements
                                    .iter()
                                    .any(|existing| existing.id == item.id)
                                    && item.posted_by != self.device_public_key
                            }) {
                                notify_room_message("Team announcement", &announcement.message);
                            }
                        }
                    }
                }
                Event::RoomRemoved(room_id) => {
                    self.rooms.retain(|room| room.id != room_id);
                    self.room_requests.retain(|request| request.room_id != room_id);
                    if self.active_room_id.as_deref() == Some(room_id.as_str()) {
                        self.active_room_id = None;
                    }
                    self.set_status("Room removed");
                }
                Event::RoomRequest {
                    room_id,
                    name,
                    from,
                } => {
                    let room_name = self
                        .rooms
                        .iter()
                        .find(|room| room.id == room_id)
                        .map(|room| room.name.clone())
                        .unwrap_or_else(|| room_id.clone());
                    self.record_audit(format!("File request in {room_name}: {name} from {from}"));
                    self.set_status(format!("{from} requested {name}"));
                    notify_room_message(
                        &format!("File request in {room_name}"),
                        &format!("{from} requested {name}"),
                    );
                    self.room_requests.push(IncomingRequest { room_id, room_name, name, from });
                    if self.room_requests.len() > 20 {
                        self.room_requests.remove(0);
                    }
                    // Pop the panel open so the request is seen even when it is tucked away.
                    self.open_for(10);
                }
                Event::Ready {
                    device_name,
                    public_key,
                } => {
                    self.record_audit(format!("Shelf started on {device_name}"));
                    self.device_name = device_name;
                    self.device_public_key = public_key;
                }
                Event::DeviceGroups(groups) => self.device_groups = groups,
                Event::ReceiveDirectory(path) => self.receive_dir = path,
                Event::Peers { paired, nearby } => {
                    self.paired = paired;
                    self.nearby = nearby;
                    match &mut self.target {
                        Target::Peer(k) => {
                            if !self.paired.iter().any(|p| &p.public_key == k) {
                                self.target = Target::All;
                            }
                        }
                        Target::Peers(keys) => {
                            keys.retain(|k| self.paired.iter().any(|p| &p.public_key == k));
                            if keys.is_empty() {
                                self.target = Target::All;
                            }
                        }
                        Target::All => {}
                    }
                }
                Event::PairingCode { code, seconds } => {
                    self.my_code = Some((code, Instant::now() + Duration::from_secs(seconds)));
                }
                Event::PairingCleared => self.my_code = None,
                Event::WebDropReady { url, seconds } => {
                    self.record_audit("Phone upload opened");
                    self.phone_upload = Some(PhoneUpload {
                        url,
                        until: Instant::now() + Duration::from_secs(seconds),
                    });
                    self.open_for(4);
                }
                Event::WebDropStopped => {
                    if self.phone_upload.take().is_some() {
                        self.record_audit("Phone upload closed");
                    }
                }
                Event::WebDropDuplicate { name, from } => {
                    self.finish_download(&name, &from);
                    self.set_status(format!("{name} is already on the shelf"));
                }
                Event::PairingResult(res) => match res {
                    Ok(name) => {
                        self.record_audit(format!("Paired with {name}"));
                        self.pair_dialog = None;
                        self.my_code = None;
                        self.set_status(format!("Paired with {name}"));
                        self.open_for(4);
                    }
                    Err(e) => {
                        self.record_audit(format!("Pairing failed: {e}"));
                        if let Some(d) = self.pair_dialog.as_mut() {
                            d.busy = false;
                            d.error = Some(e);
                        } else {
                            self.set_status(format!("Pairing failed: {e}"));
                        }
                    }
                },
                Event::Received(item) => {
                    notify_arrival(&item);
                    self.record_audit(format!("Received {} from {}", item.name, item.from));
                    if self.auto_copy {
                        clip::copy_item(&item);
                    }
                    self.set_status(format!("Received {} from {}", item.name, item.from));
                    self.finish_download(&item.name, &item.from);
                    let preview = load_item_preview(&self.ctx, &item);
                    self.items.insert(
                        0,
                        ShelfItem {
                            item,
                            received: Instant::now(),
                            preview,
                        },
                    );
                    self.open_for(4);
                }
                Event::Progress {
                    direction,
                    name,
                    peer,
                    done,
                    total,
                } => {
                    self.update_transfer(direction, name, peer, done, total);
                }
                Event::SendDone { to, item, error } => {
                    if let Some(transfer) = self.transfers.iter_mut().rev().find(|t| {
                        t.direction == TransferDirection::Upload
                            && t.name == item
                            && t.peer == to
                            && !t.finished
                    }) {
                        transfer.done = transfer.total;
                        transfer.finished = true;
                        transfer.error = error.clone();
                    }
                    match error {
                        None => {
                            self.record_audit(format!("Sent {item} to {to}"));
                            self.set_status(format!("Sent {item} to {to}"));
                        }
                        Some(e) => {
                            self.record_audit(format!("Send failed for {item} to {to}: {e}"));
                            self.set_status(format!("Could not send {item} to {to}: {e}"));
                        }
                    }
                }
                Event::Info(s) => {
                    self.record_audit(s.clone());
                    self.set_status(s);
                }
                Event::Error(s) => {
                    self.fail_latest_transfer(s.clone());
                    self.record_audit(format!("Failure: {s}"));
                    self.set_status(s);
                }
            }
        }
        while let Ok(res) = self.clip_rx.try_recv() {
            match res {
                Ok(payload) => {
                    self.set_status("Sending clipboard...");
                    self.handle.send(Command::Send {
                        target: self.target.clone(),
                        payload,
                    });
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

    fn send_paths(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        self.set_status(format!("Sending {} file(s)...", paths.len()));
        self.handle.send(Command::Send {
            target: self.target.clone(),
            payload: Payload::Files(paths),
        });
        self.hotkey_closed = false;
        self.open_for(4);
    }

    fn poll_hotkeys(&mut self) {
        while let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
            if event.state != HotKeyState::Pressed {
                continue;
            }
            if event.id == self.hotkey_toggle_id {
                self.hotkey_closed = !self.hotkey_closed;
                if !self.hotkey_closed {
                    self.open_for(8);
                }
            } else if event.id == self.hotkey_send_clipboard_id {
                self.hotkey_closed = false;
                self.open_for(3);
                self.send_clipboard();
            }
        }
    }

    /// Index into `self.monitors` of the screen the shelf sits on.
    fn monitor_index(&self) -> usize {
        if self.monitor < self.monitors.len() {
            self.monitor
        } else {
            self.monitors.iter().position(|m| m.primary).unwrap_or(0)
        }
    }

    fn apply_geometry(&mut self, ctx: &egui::Context, expanded: bool) {
        if self.monitors_checked.elapsed() > Duration::from_secs(2) {
            self.monitors = monitors::list();
            self.monitors_checked = Instant::now();
        }
        let target_w = if expanded { EXPANDED_W } else { COLLAPSED_W };
        if (self.cur_w - target_w).abs() < 0.5 {
            self.cur_w = target_w;
        } else {
            self.cur_w += (target_w - self.cur_w) * 0.35;
            ctx.request_repaint();
        }

        // Monitor rectangles are physical pixels; window commands are in egui points.
        let ppp = ctx.pixels_per_point();
        let (h, pos) = match self.monitors.get(self.monitor_index()) {
            Some(m) => {
                let h = (m.height / ppp - 120.0).clamp(460.0, 760.0);
                let x = match self.side {
                    Side::Right => m.left + m.width - self.cur_w * ppp,
                    Side::Left => m.left,
                };
                let y = m.top + (m.height - h * ppp) / 2.0;
                (h, pos2(x / ppp, y / ppp))
            }
            None => {
                let Some(monitor) = ctx.input(|i| i.viewport().monitor_size) else {
                    ctx.request_repaint_after(Duration::from_millis(100));
                    return;
                };
                let h = (monitor.y - 120.0).clamp(460.0, 760.0);
                let x = match self.side {
                    Side::Right => monitor.x - self.cur_w,
                    Side::Left => 0.0,
                };
                (h, pos2(x, (monitor.y - h) / 2.0))
            }
        };
        let size = vec2(self.cur_w, h);
        let changed = match self.last_geo {
            Some((s, p)) => (s - size).abs().max_elem() > 0.01 || (p - pos).abs().max_elem() > 0.01,
            None => true,
        };
        if changed {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
            self.last_geo = Some((size, pos));
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
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn transfer_rate(bytes_per_second: f64) -> String {
    format!("{}/s", human_size(bytes_per_second.max(0.0) as u64))
}

fn transfer_eta(seconds: f64) -> String {
    let seconds = seconds.max(0.0).ceil() as u64;
    if seconds < 60 {
        format!("{seconds}s left")
    } else {
        format!("{}m {:02}s left", seconds / 60, seconds % 60)
    }
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
    // Transparent window so the glass panel can float with rounded corners.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Rgba::TRANSPARENT.to_array()
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Follow the system light/dark setting.
        let dark = detect_dark(ctx);
        let p = match (self.material, dark) {
            (false, true) => Pal::dark(),
            (false, false) => Pal::light(),
            (true, true) => Pal::m3_dark(),
            (true, false) => Pal::m3_light(),
        };
        set_pal(p);
        if self.styled != Some((dark, self.material)) {
            apply_style(ctx, p);
            self.styled = Some((dark, self.material));
        }

        self.drain_events();
        self.poll_hotkeys();
        while let Ok(paths) = self.control_rx.try_recv() {
            self.send_paths(paths);
        }

        // Requests from the tray icon.
        if TRAY_QUIT.swap(false, Ordering::SeqCst) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if TRAY_SHOW.swap(false, Ordering::SeqCst) {
            self.hotkey_closed = false;
            self.open_for(8);
        }

        if let Some((room_id, invite_code)) = parse_room_invite(&self.room_code_input)
            .or_else(|| parse_room_invite(&self.room_id_input))
        {
            self.room_id_input = room_id;
            self.room_code_input = invite_code;
        }

        // Files dropped onto the shelf are sent to the chosen device(s).
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if !dropped.is_empty() {
            if let Some(room_id) = self.active_room_id.clone() {
                let assignment = self.room_assignment_input.trim().to_string();
                if assignment.is_empty() {
                    self.set_status(format!("Uploading {} file(s) to room...", dropped.len()));
                    self.handle.send(Command::UploadRoomFiles {
                        room_id: room_id.clone(),
                        paths: dropped,
                    });
                } else {
                    self.set_status(format!("Submitting {} file(s)...", dropped.len()));
                    self.handle.send(Command::SubmitRoomFiles {
                        room_id: room_id.clone(),
                        paths: dropped,
                        assignment: assignment.clone(),
                    });
                }
            } else {
                self.set_status(format!("Sending {} file(s)...", dropped.len()));
                self.handle.send(Command::Send {
                    target: self.target.clone(),
                    payload: Payload::Files(dropped),
                });
            }
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
        if let Some(upload) = &self.phone_upload {
            if Instant::now() > upload.until {
                self.phone_upload = None;
            } else {
                // Keep the countdown ticking.
                ctx.request_repaint_after(Duration::from_millis(500));
            }
        }

        let hovering_files = ctx.input(|i| !i.raw.hovered_files.is_empty());
        let pointer_inside = ctx.input(|i| i.pointer.hover_pos().is_some());
        if pointer_inside || hovering_files {
            self.last_active = Instant::now();
        }
        let receiving = self
            .transfers
            .iter()
            .rev()
            .find(|t| {
                t.direction == TransferDirection::Download && !t.finished && t.error.is_none()
            })
            .map(|t| Receiving {
                name: t.name.clone(),
                peer: t.peer.clone(),
                frac: if t.total == 0 {
                    None
                } else {
                    Some(t.done as f32 / t.total as f32)
                },
                speed: t.done as f64 / t.started.elapsed().as_secs_f64().max(0.001),
            });
        if receiving.is_some() {
            // Keep the panel open and the animation running while a file arrives.
            self.last_active = Instant::now();
            ctx.request_repaint_after(Duration::from_millis(33));
        }
        let expanded = !self.hotkey_closed
            && (self.pinned
                || hovering_files
                || self.last_active.elapsed() < Duration::from_millis(1200)
                || Instant::now() < self.open_until
                || self.my_code.is_some()
                || self.phone_upload.is_some()
                || self.pair_dialog.is_some());
        self.apply_geometry(ctx, expanded);

        let collapsed = self.cur_w < 80.0;
        let side = self.side;
        let current_monitor = self.monitor_index();
        let monitor_names: Vec<String> = self
            .monitors
            .iter()
            .enumerate()
            .map(|(i, m)| format!("Screen {}{}", i + 1, if m.primary { " (main)" } else { "" }))
            .collect();
        let paired = self.paired.clone();
        let nearby = self.nearby.clone();
        let device_groups = self.device_groups.clone();
        let receive_dir = self.receive_dir.clone();
        let rooms = self.rooms.clone();
        let mut actions: Vec<Action> = Vec::new();

        egui::CentralPanel::default().frame(egui::Frame::none()).show(ctx, |ui| {
            let full = ui.max_rect();

            if collapsed {
                // A slim grabber at the screen edge; it turns blue when something is waiting.
                let pill_x = match side {
                    Side::Right => full.right() - 6.0,
                    Side::Left => full.left() + 6.0,
                };
                let pill = Rect::from_center_size(Pos2::new(pill_x, full.center().y), vec2(5.0, 76.0));
                let col = if self.items.is_empty() && self.room_requests.is_empty() {
                    if p.dark { Color32::from_white_alpha(120) } else { Color32::from_black_alpha(90) }
                } else {
                    p.accent
                };
                ui.painter().rect_filled(pill, 2.5, col);
                return;
            }

            // The floating glass panel.
            let panel = Rect::from_min_max(full.min + vec2(GAP, GAP), full.max - vec2(GAP, GAP));
            let shadow = egui::Shadow {
                offset: vec2(0.0, 8.0),
                blur: 22.0,
                spread: 0.0,
                color: Color32::from_black_alpha(if p.dark { 120 } else { 55 }),
            };
            let radius = panel_radius();
            ui.painter().add(shadow.as_shape(panel, radius));
            if p.m3 {
                ui.painter().rect_filled(panel, radius, p.panel);
            } else {
                glass_backdrop(ui.painter(), panel, radius, p.dark);
            }
            if !p.m3 {
                // Inner highlight along the rim, like light catching the edge of the glass.
                ui.painter().rect_stroke(
                    panel.shrink(0.5),
                    radius,
                    Stroke::new(1.0_f32, p.panel_edge),
                );
                ui.painter().rect_stroke(
                    panel.shrink(1.5),
                    radius - 1.0,
                    Stroke::new(
                        1.0_f32,
                        if p.dark { Color32::from_white_alpha(8) } else { Color32::from_white_alpha(120) },
                    ),
                );
            }

            let inset = if p.m3 { 20.0 } else { 16.0 };
            let inner = panel.shrink2(vec2(inset, inset));
            ui.allocate_new_ui(egui::UiBuilder::new().max_rect(inner), |ui| {
                ui.set_clip_rect(inner.expand2(vec2(2.0, 0.0)));
                egui::ScrollArea::vertical()
                    .id_salt("shelf-content-scroll")
                    .auto_shrink([false, false])
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                    .max_width(inner.width())
                    .show(ui, |ui| {
                    ui.set_width(inner.width().max(1.0));
                    if p.m3 {
                        m3_receive_notch(ui, receiving.as_ref());
                    }
                    // Header.
                    ui.horizontal(|ui| {
                        match &self.logo {
                            Some(tex) if p.m3 => {
                                ui.add(egui::Image::new(egui::load::SizedTexture::new(
                                    tex.id(),
                                    vec2(34.0, 34.0),
                                )));
                            }
                            Some(tex) => {
                                // Glass plate behind the logo, like the mockup.
                                let (plate, _) =
                                    ui.allocate_exact_size(vec2(38.0, 38.0), egui::Sense::hover());
                                ui.painter().rect_filled(plate, 12.0, p.card);
                                ui.painter().rect_stroke(
                                    plate.shrink(0.5),
                                    12.0,
                                    Stroke::new(1.0_f32, p.card_edge),
                                );
                                egui::Image::new(egui::load::SizedTexture::new(
                                    tex.id(),
                                    vec2(30.0, 30.0),
                                ))
                                .paint_at(ui, Rect::from_center_size(plate.center(), vec2(30.0, 30.0)));
                            }
                            None => {
                                ui.allocate_space(vec2(34.0, 34.0));
                            }
                        }
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            if !self.device_name.is_empty() {
                                if p.m3 {
                                    ui.label(
                                        RichText::new(shorten(&self.device_name, 22))
                                            .size(17.0)
                                            .color(p.text),
                                    );
                                } else {
                                    ui.label(
                                        semibold(shorten(&self.device_name, 22), 16.0)
                                            .color(p.text),
                                    );
                                }
                            }
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if close_button(ui).clicked() {
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                            // The pin lives in the quick-action row.
                        });
                    });

                    if !self.status.is_empty() {
                        status_pill(ui, &self.status);
                    }
                    {
                        // Home layout shared by the macOS glass and Material 3 styles: drop pill, Clipboard / Devices tiles, transfers,
                        // round quick actions and the All settings switch.
                        drop_zone(ui, hovering_files);
                        let tile_gap = 8.0;
                        let tile_w = ((ui.available_width() - tile_gap) / 2.0).floor();
                        let online = paired.iter().filter(|d| d.addr.is_some()).count();
                        let online_label = format!("{online} {}", tr("online"));
                        let clip_glyph: Glyph = if p.m3 { glyph_doc } else { glyph_copy };
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = tile_gap;
                            if glass_tile(ui, tile_w, 80.0, clip_glyph, tr("Clipboard"), tr("Send"), 0.0, true, false)
                                .clicked()
                            {
                                actions.push(Action::SendClipboard);
                            }
                            if glass_tile(ui, tile_w, 80.0, glyph_airplay, tr("Devices"), &online_label, 0.0, true, false)
                                .clicked()
                            {
                                self.show_all_settings = true;
                                self.devices_focus = true;
                            }
                        });
                        if let Some(upload) = &self.phone_upload {
                            let left = upload.until.saturating_duration_since(Instant::now()).as_secs();
                            let url = upload.url.clone();
                            card(ui, |ui| {
                                ui.vertical_centered(|ui| {
                                    ui.label(semibold(tr("Send from your phone"), 14.0).color(p.text));
                                    ui.label(
                                        RichText::new(tr("Scan with the phone camera. Both devices need the same Wi-Fi."))
                                            .size(12.0)
                                            .color(p.text2),
                                    );
                                    ui.add_space(4.0);
                                    draw_qr(ui, &url, 176.0, 2);
                                    ui.add_space(4.0);
                                    ui.label(RichText::new(&url).size(10.5).color(p.text3));
                                    ui.label(
                                        RichText::new(format!("{} {}:{:02}", tr("Closes in"), left / 60, left % 60))
                                            .size(11.5)
                                            .color(p.text3),
                                    );
                                    ui.horizontal(|ui| {
                                        if capsule(ui, tr("Copy link"), Look::Glass, 28.0, None, true).clicked() {
                                            let _ = clip::copy_text(&url);
                                            self.status = "Link copied".to_string();
                                        }
                                        if capsule(ui, tr("Stop"), Look::Glass, 28.0, None, true).clicked() {
                                            actions.push(Action::TogglePhoneUpload);
                                        }
                                    });
                                    ui.label(
                                        RichText::new(tr("Not encrypted. Use it on a network you trust."))
                                            .size(11.0)
                                            .color(p.text3),
                                    );
                                });
                            });
                        }
                        transfers_summary(ui, &self.transfers);
                        transfer_cards(ui, &self.transfers, &mut actions);

                        let circle = ((ui.available_width() - 30.0) / 4.0).floor().clamp(52.0, 76.0);
                        let circle_gap = ((ui.available_width() - circle * 4.0) / 3.0).max(6.0);
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = circle_gap;
                            if quick_circle(ui, circle, glyph_gear, self.show_all_settings)
                                .on_hover_text(tr("All settings"))
                                .clicked()
                            {
                                self.show_all_settings = !self.show_all_settings;
                            }
                            if quick_circle(ui, circle, glyph_qr, self.phone_upload.is_some())
                                .on_hover_text(tr("Phone upload"))
                                .clicked()
                            {
                                actions.push(Action::TogglePhoneUpload);
                            }
                            if quick_circle(ui, circle, glyph_pause, self.receiving_paused)
                                .on_hover_text("Pause receiving")
                                .clicked()
                            {
                                self.receiving_paused = !self.receiving_paused;
                                self.handle
                                    .send(Command::SetReceivingPaused(self.receiving_paused));
                            }
                            if quick_circle(ui, circle, glyph_pin, self.pinned)
                                .on_hover_text(if self.pinned { "Pinned open" } else { "Keep open" })
                                .clicked()
                            {
                                self.pinned = !self.pinned;
                            }
                        });
                        let settings_label = if self.show_all_settings {
                            tr("Hide settings")
                        } else {
                            tr("All settings")
                        };
                        let settings_pill = if p.m3 {
                            icon_pill(ui, glyph_sliders, settings_label, 42.0)
                        } else {
                            capsule(ui, settings_label, Look::Glass, 36.0, None, true)
                        };
                        if settings_pill.clicked() {
                            self.show_all_settings = !self.show_all_settings;
                        }
                    }
                    let show_more = p.m3
                        || self.show_all_settings
                        || self.my_code.is_some()
                        || self.pair_dialog.is_some();

                    // Settings.
                    card(ui, |ui| {
                        ui.label(RichText::new(tr("Send to")).size(13.5).color(p.text));
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                            let all = matches!(self.target, Target::All);
                            if capsule(ui, tr("All devices"), Look::Chip(all), 28.0, None, true).clicked() {
                                self.target = Target::All;
                            }
                            for dev in &paired {
                                let selected = match &self.target {
                                    Target::All => false,
                                    Target::Peer(k) => k == &dev.public_key,
                                    Target::Peers(keys) => keys.iter().any(|k| k == &dev.public_key),
                                };
                                if capsule(ui, &shorten(&dev.name, 18), Look::Chip(selected), 28.0, None, true)
                                    .clicked()
                                {
                                    let mut keys = match &self.target {
                                        Target::All => Vec::new(),
                                        Target::Peer(k) => vec![k.clone()],
                                        Target::Peers(keys) => keys.clone(),
                                    };
                                    if let Some(index) =
                                        keys.iter().position(|k| k == &dev.public_key)
                                    {
                                        keys.remove(index);
                                    } else {
                                        keys.push(dev.public_key.clone());
                                    }
                                    self.target = match keys.len() {
                                        0 => Target::All,
                                        1 => Target::Peer(keys.remove(0)),
                                        _ => Target::Peers(keys),
                                    };
                                }
                            }
                        });
                        hairline(ui);
                        row_toggle(
                            ui,
                            "Auto-copy received",
                            "Ready to paste with Ctrl+V",
                            &mut self.auto_copy,
                        );
                        let was_paused = self.receiving_paused;
                        row_toggle(
                            ui,
                            "Pause receiving",
                            "Outgoing transfers will continue",
                            &mut self.receiving_paused,
                        );
                        if self.receiving_paused != was_paused {
                            self.handle
                                .send(Command::SetReceivingPaused(self.receiving_paused));
                        }
                        hairline(ui);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Download location").size(13.5).color(p.text));
                            let choose = if p.m3 {
                                icon_pill(ui, glyph_folder, "Choose folder", 40.0)
                            } else {
                                capsule(ui, "Choose folder", Look::Glass, 28.0, None, true)
                            };
                            if choose.clicked() {
                                actions.push(Action::ChooseReceiveFolder);
                            }
                        });
                        let dir_text = receive_dir
                            .as_ref()
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| shelf_core::inbox_dir().display().to_string());
                        ui.horizontal(|ui| {
                            if p.m3 {
                                let (r, _) = ui.allocate_exact_size(vec2(22.0, 22.0), egui::Sense::hover());
                                glyph_folder(ui.painter(), r.center(), p.text2);
                            }
                            ui.label(RichText::new(dir_text).size(10.5).color(p.text2));
                        });
                        if show_more {
                        hairline(ui);
                        ui.label(RichText::new("Device groups").size(13.5).color(p.text));
                        ui.horizontal_wrapped(|ui| {
                            for group in &device_groups {
                                if capsule(ui, &group.name, Look::Chip(false), 28.0, None, true).clicked() {
                                    actions.push(Action::UseDeviceGroup(group.clone()));
                                }
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.group_name_input)
                                    .hint_text("New group name")
                                    .desired_width((ui.available_width() - 108.0).max(60.0)),
                            );
                            if capsule(ui, "Save group", Look::Glass, 28.0, Some(88.0), !self.group_name_input.trim().is_empty()).clicked() {
                                actions.push(Action::SaveDeviceGroup);
                            }
                        });
                        hairline(ui);
                        ui.label(RichText::new("Style").size(13.5).color(p.text));
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                            if capsule(ui, "macOS", Look::Chip(!self.material), 28.0, None, true).clicked() {
                                self.material = false;
                            }
                            if capsule(ui, "Material 3", Look::Chip(self.material), 28.0, None, true).clicked() {
                                self.material = true;
                            }
                        });
                        hairline(ui);
                        ui.label(RichText::new("Screen edge").size(13.5).color(p.text));
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                            for (s, label) in [(Side::Left, "Left"), (Side::Right, "Right")] {
                                if capsule(ui, label, Look::Chip(self.side == s), 28.0, None, true).clicked() {
                                    self.side = s;
                                }
                            }
                            if monitor_names.len() > 1 {
                                for (i, name) in monitor_names.iter().enumerate() {
                                    if capsule(ui, name, Look::Chip(current_monitor == i), 28.0, None, true).clicked() {
                                        self.monitor = i;
                                    }
                                }
                            }
                        });
                        }
                    });

                    if show_more {
                    // Collaboration rooms.
                    egui::CollapsingHeader::new(semibold(tr("Rooms"), 12.5).color(p.text2))
                        .default_open(true)
                        .show(ui, |ui| {
                            card(ui, |ui| {
                                if !self.room_requests.is_empty() {
                                    ui.label(
                                        RichText::new(format!("File requests ({})", self.room_requests.len()))
                                            .size(12.0)
                                            .color(p.accent),
                                    );
                                    for (i, request) in self.room_requests.iter().enumerate() {
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(format!(
                                                    "{} asked for \"{}\" in {}",
                                                    request.from,
                                                    shorten(&request.name, 40),
                                                    shorten(&request.room_name, 24)
                                                ))
                                                .size(11.5)
                                                .color(p.text),
                                            )
                                            .wrap(),
                                        );
                                        ui.horizontal(|ui| {
                                            if capsule(ui, "Send file", Look::Primary, 24.0, None, true).clicked() {
                                                actions.push(Action::FulfillRequest(i));
                                            }
                                            if capsule(ui, "Dismiss", Look::Glass, 24.0, None, true).clicked() {
                                                actions.push(Action::DismissRequest(i));
                                            }
                                        });
                                    }
                                    hairline(ui);
                                }
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.room_name_input)
                                            .hint_text(tr("New class or team room"))
                                            .desired_width((ui.available_width() - 100.0).max(60.0)),
                                    );
                                    if capsule(
                                        ui,
                                        tr("Create"),
                                        Look::Primary,
                                        28.0,
                                        Some(74.0),
                                        !self.room_name_input.trim().is_empty(),
                                    )
                                    .clicked()
                                    {
                                        actions.push(Action::CreateRoom);
                                    }
                                });
                                ui.horizontal(|ui| {
                                    if capsule(ui, "Choose assignment file", Look::Glass, 28.0, None, true).clicked() {
                                        actions.push(Action::ChooseAssignmentFiles);
                                    }
                                    if !self.assignment_files.is_empty() {
                                        ui.label(format!("{} file(s) selected", self.assignment_files.len()));
                                    }
                                });
                                ui.horizontal(|ui| {
                                    if capsule(ui, "Restore room", Look::Glass, 28.0, None, true).clicked() {
                                        actions.push(Action::ImportRoom);
                                    }
                                });
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::TextEdit::singleline(
                                            &mut self.room_assignment_input,
                                        )
                                        .hint_text("Assignment name for dropped files")
                                        .desired_width((ui.available_width() - 100.0).max(60.0)),
                                    );
                                    if capsule(
                                        ui,
                                        tr("Submit"),
                                        Look::Primary,
                                        28.0,
                                        Some(74.0),
                                        self.active_room_id.is_some()
                                            && !self.room_assignment_input.trim().is_empty(),
                                    )
                                    .clicked()
                                    {
                                        if let Some(room_id) = self.active_room_id.clone() {
                                            actions.push(Action::SubmitRoomFile { room_id });
                                        }
                                    }
                                });
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.room_request_input)
                                            .hint_text(tr("Request a file from the room"))
                                            .desired_width((ui.available_width() - 100.0).max(60.0)),
                                    );
                                    if capsule(
                                        ui,
                                        tr("Request"),
                                        Look::Glass,
                                        28.0,
                                        Some(74.0),
                                        !self.room_request_input.trim().is_empty()
                                            && !rooms.is_empty(),
                                    )
                                    .clicked()
                                    {
                                        if let Some(room) = self.active_room_id.as_ref().and_then(|id| rooms.iter().find(|room| &room.id == id)).or_else(|| rooms.first()) {
                                            actions.push(Action::RequestRoomFile {
                                                room_id: room.id.clone(),
                                            });
                                        }
                                    }
                                });
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.room_announcement_input)
                                            .hint_text(tr("Team announcement"))
                                            .desired_width((ui.available_width() - 100.0).max(60.0)),
                                    );
                                    if capsule(
                                        ui,
                                        tr("Announce"),
                                        Look::Primary,
                                        28.0,
                                        Some(74.0),
                                        self.active_room_id.is_some()
                                            && !self.room_announcement_input.trim().is_empty(),
                                    )
                                    .clicked()
                                    {
                                        if let Some(room_id) = self.active_room_id.clone() {
                                            actions.push(Action::UpdateRoom {
                                                room_id,
                                                update: RoomUpdate::AddAnnouncement {
                                                    message: self.room_announcement_input.trim().to_string(),
                                                    pinned: true,
                                                },
                                            });
                                        }
                                    }
                                });
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.room_clipboard_input)
                                            .hint_text(tr("Shared clipboard entry"))
                                            .desired_width((ui.available_width() - 100.0).max(60.0)),
                                    );
                                    if capsule(
                                        ui,
                                        tr("Share text"),
                                        Look::Glass,
                                        28.0,
                                        Some(74.0),
                                        self.active_room_id.is_some()
                                            && !self.room_clipboard_input.trim().is_empty(),
                                    )
                                    .clicked()
                                    {
                                        if let Some(room_id) = self.active_room_id.clone() {
                                            actions.push(Action::UpdateRoom {
                                                room_id,
                                                update: RoomUpdate::AddClipboard {
                                                    content: self.room_clipboard_input.trim().to_string(),
                                                },
                                            });
                                        }
                                    }
                                });
                                ui.vertical(|ui| {
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.room_note_title_input)
                                            .hint_text("Note title")
                                            .desired_width(ui.available_width()),
                                    );
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.room_note_body_input)
                                            .hint_text("Shared note")
                                            .desired_width(ui.available_width()),
                                    );
                                    if capsule(ui, tr("Add note"), Look::Glass, 28.0, Some(ui.available_width()), self.active_room_id.is_some()).clicked() {
                                        if let Some(room_id) = self.active_room_id.clone() {
                                            actions.push(Action::UpdateRoom {
                                                room_id,
                                                update: RoomUpdate::AddNote {
                                                    title: self.room_note_title_input.trim().to_string(),
                                                    body: self.room_note_body_input.trim().to_string(),
                                                },
                                            });
                                        }
                                    }
                                });
                                ui.vertical(|ui| {
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.room_link_title_input)
                                            .hint_text("Link title")
                                            .desired_width(ui.available_width()),
                                    );
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.room_link_url_input)
                                            .hint_text("https:// shared link")
                                            .desired_width(ui.available_width()),
                                    );
                                    if capsule(ui, tr("Add link"), Look::Glass, 28.0, Some(ui.available_width()), self.active_room_id.is_some()).clicked() {
                                        if let Some(room_id) = self.active_room_id.clone() {
                                            actions.push(Action::UpdateRoom {
                                                room_id,
                                                update: RoomUpdate::AddLink {
                                                    title: self.room_link_title_input.trim().to_string(),
                                                    url: self.room_link_url_input.trim().to_string(),
                                                },
                                            });
                                        }
                                    }
                                });
                                ui.vertical(|ui| {
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.room_id_input)
                                            .hint_text("Room ID")
                                            .desired_width(ui.available_width()),
                                    );
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.room_code_input)
                                            .hint_text("Invite code")
                                            .desired_width(ui.available_width()),
                                    );
                                });
                                for device in &paired {
                                    if let Some(addr) = device.addr {
                                        if capsule(
                                            ui,
                                            &format!("Join on {}", shorten(&device.name, 24)),
                                            Look::Glass,
                                            28.0,
                                            Some(ui.available_width()),
                                            !self.room_id_input.trim().is_empty()
                                                && !self.room_code_input.trim().is_empty(),
                                        )
                                        .clicked()
                                        {
                                            actions.push(Action::JoinRoom(addr));
                                        }
                                    }
                                }
                                if rooms.is_empty() {
                                    ui.label(
                                        RichText::new(
                                            "Create a room to share work with classmates or teammates.",
                                        )
                                        .size(12.0)
                                        .color(p.text2),
                                    );
                                } else {
                                    for room in &rooms {
 ui.push_id(room.id.clone(), |ui| {
                                        ui.set_width(ui.available_width());
                                        hairline(ui);
                                        ui.vertical(|ui| {
                                            ui.set_width(ui.available_width());
                                            ui.vertical(|ui| {
                                                ui.label(
                                                    RichText::new(&room.name)
                                                        .size(13.5)
                                                        .color(p.text),
                                                );
                                                let role = if room.owner_public_key
                                                    == self.device_public_key
                                                {
                                                    "Owner"
                                                } else {
                                                    "Member"
                                                };
                                                ui.label(
                                                    RichText::new(format!(
                                                        "{} · {} member(s)",
                                                        role,
                                                        room.members.len()
                                                    ))
                                                    .size(11.5)
                                                    .color(p.text2),
                                                );
                                                if role == "Owner" {
                                                    ui.label(
                                                        RichText::new(format!(
                                                            "Room ID: {}",
                                                            room.id
                                                        ))
                                                        .size(11.5)
                                                        .color(p.text2),
                                                    );
                                                    ui.label(
                                                        RichText::new(format!(
                                                            "Invite code: {}",
                                                            room.invite_code
                                                        ))
                                                        .size(11.5)
                                                        .color(p.accent),
                                                    );
                                                    ui.add(
                                                        egui::Label::new(
                                                            RichText::new(format!(
                                                                "Invite link: shelf://room/{}/{}",
                                                                room.id, room.invite_code
                                                            ))
                                                            .size(10.5)
                                                            .color(p.text2),
                                                        )
                                                        .wrap(),
                                                    );
                                                    room_qr(ui, room);
                                                }
                                            });
                                            let active =
                                                self.active_room_id.as_deref() == Some(room.id.as_str());
                                            if capsule(
                                                ui,
                                                if active { "Dropbox active" } else { "Use dropbox" },
                                                if active { Look::Primary } else { Look::Glass },
                                                26.0,
                                                Some(ui.available_width()),
                                                true,
                                            )
                                            .clicked()
                                            {
                                                    self.active_room_id = Some(room.id.clone());
                                            }
                                        });
                                        for file in room.files.iter().take(20) {
 ui.push_id(file.hash.clone(), |ui| {
                                            ui.horizontal_wrapped(|ui| {
                                                ui.label(
                                                    RichText::new(format!(
                                                        "{} · {} · v{}",
                                                        shorten(&file.name, 28),
                                                        human_size(file.size),
                                                        file.version
                                                    ))
                                                    .size(11.5)
                                                    .color(p.text2),
                                                );
                                                if let Some(locker) = &file.locked_by {
                                                    ui.label(
                                                        RichText::new(format!(
                                                            "Locked by {}",
                                                            shorten(locker, 10)
                                                        ))
                                                        .size(10.5)
                                                        .color(p.orange),
                                                    );
                                                }
                                                let can_edit = room.can_edit(&self.device_public_key);
                                                let mine = file.locked_by.as_deref() == Some(self.device_public_key.as_str());
                                                if can_edit
                                                    && (file.locked_by.is_none() || mine)
                                                    && capsule(
                                                        ui,
                                                        if mine { tr("Unlock") } else { tr("Lock") },
                                                        Look::Glass,
                                                        22.0,
                                                        None,
                                                        true,
                                                    )
                                                    .clicked()
                                                {
                                                    actions.push(Action::UpdateRoom {
                                                        room_id: room.id.clone(),
                                                        update: if mine {
                                                            RoomUpdate::UnlockFile { hash: file.hash.clone() }
                                                        } else {
                                                            RoomUpdate::LockFile { hash: file.hash.clone() }
                                                        },
                                                    });
                                                }
                                                if room.owner_public_key == self.device_public_key
                                                    && capsule(
                                                        ui,
                                                        tr("Delete"),
                                                        Look::Tint(p.red),
                                                        24.0,
                                                        None,
                                                        true,
                                                    )
                                                    .clicked()
                                                {
                                                    actions.push(Action::DeleteRoomFile {
                                                        room_id: room.id.clone(),
                                                        hash: file.hash.clone(),
                                                    });
                                                }
                                                if room.can_edit(&self.device_public_key)
                                                    && capsule(
                                                        ui,
                                                        if file.downloads_allowed { "Restrict" } else { "Allow" },
                                                        Look::Glass,
                                                        24.0,
                                                        None,
                                                        true,
                                                    )
                                                    .clicked()
                                                {
                                                    actions.push(Action::UpdateRoom {
                                                        room_id: room.id.clone(),
                                                        update: RoomUpdate::SetFileDownloads {
                                                            hash: file.hash.clone(),
                                                            allowed: !file.downloads_allowed,
                                                        },
                                                    });
                                                }
                                            });
                                        });
 }
                                        if !room.announcements.is_empty() {
                                            ui.add_space(4.0);
                                            ui.label(RichText::new(tr("Announcements")).size(12.0).color(p.text2));
                                            for announcement in room.announcements.iter().rev().take(5) {
                                                ui.horizontal_wrapped(|ui| {
                                                    ui.add(
                                                        egui::Label::new(
                                                            RichText::new(format!(
                                                                "{}{}",
                                                                if announcement.pinned { "📌 " } else { "" },
                                                                announcement.message
                                                            ))
                                                            .size(11.5)
                                                            .color(p.text),
                                                        )
                                                        .wrap(),
                                                    );
                                                    if room.can_edit(&self.device_public_key)
                                                        && capsule(
                                                            ui,
                                                            if announcement.pinned { "Unpin" } else { "Pin" },
                                                            Look::Glass,
                                                            22.0,
                                                            None,
                                                            true,
                                                        )
                                                        .clicked()
                                                    {
                                                        actions.push(Action::UpdateRoom {
                                                            room_id: room.id.clone(),
                                                            update: RoomUpdate::SetAnnouncementPinned {
                                                                id: announcement.id.clone(),
                                                                pinned: !announcement.pinned,
                                                            },
                                                        });
                                                    }
                                                });
                                            }
                                        }
                                        if !room.clipboard.is_empty() {
                                            ui.add_space(4.0);
                                            ui.label(RichText::new(tr("Shared clipboard")).size(12.0).color(p.text2));
                                            for entry in room.clipboard.iter().rev().take(5) {
                                                ui.horizontal_wrapped(|ui| {
                                                    ui.add(
                                                        egui::Label::new(
                                                            RichText::new(shorten(&entry.content, 42))
                                                                .size(11.5)
                                                                .color(p.text),
                                                        )
                                                        .wrap(),
                                                    );
                                                    if capsule(ui, tr("Copy"), Look::Glass, 22.0, None, true).clicked() {
                                                        let _ = clip::copy_text(&entry.content);
                                                    }
                                                    if room.can_edit(&self.device_public_key)
                                                        && capsule(ui, tr("Remove"), Look::Tint(p.red), 22.0, None, true).clicked()
                                                    {
                                                        actions.push(Action::UpdateRoom {
                                                            room_id: room.id.clone(),
                                                            update: RoomUpdate::RemoveClipboard { id: entry.id.clone() },
                                                        });
                                                    }
                                                });
                                            }
                                        }
                                        if !room.activity.is_empty() {
                                            ui.add_space(4.0);
                                            ui.label(RichText::new(tr("Recent activity")).size(12.0).color(p.text2));
                                            for activity in room.activity.iter().rev().take(5) {
                                                ui.label(
                                                    RichText::new(format!(
                                                        "{} · {}",
                                                        shorten(&activity.actor, 10),
                                                        activity.message
                                                    ))
                                                    .size(10.5)
                                                    .color(p.text2),
                                                );
                                            }
                                        }
                                        if !room.notes.is_empty() {
                                            ui.add_space(4.0);
                                            ui.label(RichText::new(tr("Shared notes")).size(12.0).color(p.text2));
                                            for note in room.notes.iter().rev().take(5) {
                                                ui.add(
                                                    egui::Label::new(
                                                        RichText::new(format!("{}: {}", note.title, note.body))
                                                            .size(11.5)
                                                            .color(p.text),
                                                    )
                                                    .wrap(),
                                                );
                                            }
                                        }
                                        if !room.links.is_empty() {
                                            ui.add_space(4.0);
                                            ui.label(RichText::new(tr("Shared links")).size(12.0).color(p.text2));
                                            for link in room.links.iter().rev().take(5) {
                                                ui.hyperlink_to(
                                                    RichText::new(&link.title).size(11.5),
                                                    &link.url,
                                                );
                                            }
                                        }
                                        if let Some(expires_at) = room.session_expires_at {
                                            ui.label(
                                                RichText::new(format!("Room expires at Unix time {}", expires_at))
                                                    .size(10.5)
                                                    .color(p.orange),
                                            );
                                        }
                                        if room.owner_public_key != self.device_public_key {
                                            if !room.can_edit(&self.device_public_key) {
                                                ui.label(
                                                    RichText::new(
                                                        "Only the owner and editors can lock, restrict or delete files.",
                                                    )
                                                    .size(10.5)
                                                    .color(p.text3),
                                                );
                                            }
                                            if capsule(ui, "Leave room", Look::Tint(p.red), 28.0, Some(ui.available_width()), true).clicked() {
                                                actions.push(Action::LeaveRoom(room.id.clone()));
                                            }
                                        }
                                        if room.owner_public_key == self.device_public_key {
                                            if capsule(ui, "Delete room", Look::Tint(p.red), 28.0, Some(ui.available_width()), true).clicked() {
                                                actions.push(Action::DeleteRoom(room.id.clone()));
                                            }
                                            if capsule(ui, "Export backup", Look::Glass, 28.0, Some(ui.available_width()), true).clicked() {
                                                actions.push(Action::ExportRoom(room.id.clone()));
                                            }
                                            ui.horizontal(|ui| {
                                                ui.add(
                                                    egui::TextEdit::singleline(&mut self.room_expiration_input)
                                                        .hint_text("Expiration Unix seconds (blank = none)")
                                                        .desired_width((ui.available_width() - 108.0).max(60.0)),
                                                );
                                                if capsule(ui, tr("Set expiry"), Look::Glass, 28.0, Some(88.0), true).clicked() {
                                                    let expires_at = self.room_expiration_input.trim().parse::<u64>().ok();
                                                    actions.push(Action::UpdateRoom {
                                                        room_id: room.id.clone(),
                                                        update: RoomUpdate::SetSessionExpiration { expires_at },
                                                    });
                                                }
                                            });
                                            ui.add_space(4.0);
                                            ui.label(RichText::new(tr("Members and roles")).size(12.0).color(p.text2));
                                            for member in room.members.iter().filter(|member| member.public_key != room.owner_public_key) {
                                                ui.horizontal(|ui| {
                                                    ui.label(RichText::new(shorten(&member.name, 18)).size(11.5).color(p.text));
                                                    let mut selected = member.role;
                                                    egui::ComboBox::from_id_salt(format!("role-{}-{}", room.id, member.public_key))
                                                        .selected_text(format!("{:?}", selected))
                                                        .show_ui(ui, |ui| {
                                                            for role in [RoomRole::Member, RoomRole::Editor, RoomRole::Contributor, RoomRole::Viewer] {
                                                                ui.selectable_value(&mut selected, role, format!("{:?}", role));
                                                            }
                                                        });
                                                    if selected != member.role {
                                                        actions.push(Action::UpdateRoom {
                                                            room_id: room.id.clone(),
                                                            update: RoomUpdate::SetMemberRole {
                                                                public_key: member.public_key.clone(),
                                                                role: selected,
                                                            },
                                                        });
                                                    }
                                                });
                                            }
                                            ui.add_space(4.0);
                                            ui.label(
                                                RichText::new(tr("Teacher assignment"))
                                                    .size(12.0)
                                                    .color(p.text2),
                                            );
                                            ui.add(
                                                egui::TextEdit::singleline(
                                                    &mut self.assignment_title_input,
                                                )
                                                .hint_text("Title"),
                                            );
                                            ui.add(
                                                egui::TextEdit::singleline(
                                                    &mut self.assignment_deadline_input,
                                                )
                                                .hint_text("Deadline (Unix seconds)"),
                                            );
                                            ui.add(
                                                egui::TextEdit::multiline(
                                                    &mut self.assignment_instructions_input,
                                                )
                                                .hint_text("Instructions")
                                                .desired_rows(2),
                                            );
                                            if capsule(
                                                ui,
                                                tr("Create assignment"),
                                                Look::Primary,
                                                28.0,
                                                Some(ui.available_width()),
                                                !self.assignment_title_input.trim().is_empty(),
                                            )
                                            .clicked()
                                            {
                                                actions.push(Action::CreateAssignment {
                                                    room_id: room.id.clone(),
                                                });
                                            }
                                            if !self.assignment_files.is_empty() {
                                                ui.label(RichText::new(format!("Assignment files: {}", self.assignment_files.iter().filter_map(|path| path.file_name()).map(|name| name.to_string_lossy()).collect::<Vec<_>>().join(", "))).size(11.0).color(p.text2));
                                            }
                                        }
                                        for assignment in &room.assignments {
                                            let submitted = assignment.submissions.len();
                                            let expected = room
                                                .members
                                                .iter()
                                                .filter(|member| {
                                                    member.public_key != room.owner_public_key
                                                })
                                                .count();
                                            ui.label(
                                                RichText::new(format!(
                                                    "{} · {}/{} submitted · deadline {}",
                                                    assignment.title,
                                                    submitted,
                                                    expected,
                                                    assignment.deadline
                                                ))
                                                .size(11.5)
                                                .color(p.text2),
                                            );
                                        }
                                    });
 }
                                }
                            });
                        });

                    // Devices and pairing.
                    egui::CollapsingHeader::new(semibold(tr("Devices"), 12.5).color(p.text2))
                        .default_open(
                            paired.is_empty() || self.my_code.is_some() || self.pair_dialog.is_some(),
                        )
                        .open(if self.devices_focus { Some(true) } else { None })
                        .show(ui, |ui| {
                            if paired.is_empty() && nearby.is_empty() {
                                ui.label(
                                    RichText::new(
                                        "No devices yet. Open Shelf on your other PC, then pair below.",
                                    )
                                    .size(12.0)
                                    .color(p.text2),
                                );
                            }
                            if !paired.is_empty() {
                                card(ui, |ui| {
                                    for (i, dev) in paired.iter().enumerate() {
                                        if i > 0 {
                                            hairline(ui);
                                        }
                                        let online = dev.addr.is_some();
                                        ui.horizontal(|ui| {
                                            let (r, _) = ui.allocate_exact_size(
                                                vec2(34.0, 34.0),
                                                egui::Sense::hover(),
                                            );
                                            draw_device_icon(ui.painter(), r, online);
                                            ui.vertical(|ui| {
                                                ui.spacing_mut().item_spacing.y = 1.0;
                                                ui.label(
                                                    RichText::new(shorten(&dev.name, 20))
                                                        .size(13.5)
                                                        .color(p.text),
                                                );
                                                ui.label(
                                                    RichText::new(if online {
                                                        "Online"
                                                    } else {
                                                        "Offline"
                                                    })
                                                    .size(11.5)
                                                    .color(if online { p.good } else { p.text3 }),
                                                );
                                            });
                                            ui.with_layout(
                                                Layout::right_to_left(Align::Center),
                                                |ui| {
                                                    if capsule(
                                                        ui,
                                                        "Remove",
                                                        Look::Tint(p.red),
                                                        26.0,
                                                        None,
                                                        true,
                                                    )
                                                    .clicked()
                                                    {
                                                        actions.push(Action::Unpair(
                                                            dev.public_key.clone(),
                                                        ));
                                                    }
                                                },
                                            );
                                        });
                                    }
                                });
                            }
                            if !nearby.is_empty() {
                                section_label(ui, tr("Nearby"));
                                card(ui, |ui| {
                                    for (i, d) in nearby.iter().enumerate() {
                                        if i > 0 {
                                            hairline(ui);
                                        }
                                        ui.horizontal(|ui| {
                                            let (r, _) = ui.allocate_exact_size(
                                                vec2(34.0, 34.0),
                                                egui::Sense::hover(),
                                            );
                                            draw_device_icon(ui.painter(), r, false);
                                            ui.vertical(|ui| {
                                                ui.spacing_mut().item_spacing.y = 1.0;
                                                ui.label(
                                                    RichText::new(shorten(&d.name, 20))
                                                        .size(13.5)
                                                        .color(p.text),
                                                );
                                                ui.label(
                                                        RichText::new(tr("Not paired"))
                                                        .size(11.5)
                                                        .color(p.text3),
                                                );
                                            });
                                            ui.with_layout(
                                                Layout::right_to_left(Align::Center),
                                                |ui| {
                                                    if capsule(
                                                        ui,
                                                        tr("Pair"),
                                                        Look::Primary,
                                                        26.0,
                                                        None,
                                                        true,
                                                    )
                                                    .clicked()
                                                    {
                                                        actions.push(Action::OpenPair(d.clone()));
                                                    }
                                                },
                                            );
                                        });
                                    }
                                });
                            }

                            if let Some((code, until)) = &self.my_code {
                                let left =
                                    until.saturating_duration_since(Instant::now()).as_secs();
                                card(ui, |ui| {
                                    ui.vertical_centered(|ui| {
                                        ui.label(
                                            RichText::new("Enter this code on the other PC")
                                                .size(12.5)
                                                .color(p.text2),
                                        );
                                        ui.add_space(2.0);
                                        code_tiles(ui, code);
                                        ui.add_space(2.0);
                                        thin_progress(ui, left as f32 / 120.0);
                                        ui.label(
                                            RichText::new(format!("Expires in {left}s"))
                                                .size(11.5)
                                                .color(p.text3),
                                        );
                                        if capsule(ui, "Cancel", Look::Glass, 28.0, None, true)
                                            .clicked()
                                        {
                                            actions.push(Action::CancelPairing);
                                        }
                                    });
                                });
                            } else if self.pair_dialog.is_none()
                                && capsule(
                                    ui,
                                            tr("Show pairing code"),
                                    Look::Glass,
                                    34.0,
                                    Some(ui.available_width()),
                                    true,
                                )
                                .clicked()
                            {
                                actions.push(Action::StartPairing);
                            }

                            if let Some(d) = self.pair_dialog.as_mut() {
                                card(ui, |ui| {
                                    ui.label(
                                        semibold(
                                            format!("Pair with {}", shorten(&d.device.name, 22)),
                                            14.0,
                                        )
                                        .color(p.text),
                                    );
                                    ui.label(
                                        RichText::new("Type the 6 digit code shown on that PC.")
                                            .size(12.0)
                                            .color(p.text2),
                                    );
                                    ui.add(
                                        egui::TextEdit::singleline(&mut d.code)
                                            .font(sb_font(24.0))
                                            .horizontal_align(Align::Center)
                                            .desired_width(ui.available_width())
                                            .margin(vec2(8.0, 10.0))
                                            .char_limit(6)
                                            .hint_text("000000"),
                                    );
                                    if let Some(e) = &d.error {
                                        ui.label(RichText::new(e.clone()).size(12.0).color(p.red));
                                    }
                                    ui.horizontal(|ui| {
                                        let valid = d.code.len() == 6
                                            && d.code.chars().all(|c| c.is_ascii_digit());
                                        if capsule(
                                            ui,
                                            tr("Connect"),
                                            Look::Primary,
                                            30.0,
                                            None,
                                            valid && !d.busy,
                                        )
                                        .clicked()
                                        {
                                            actions.push(Action::ConnectPair);
                                        }
                                        if capsule(ui, tr("Cancel"), Look::Glass, 30.0, None, true)
                                            .clicked()
                                        {
                                            actions.push(Action::ClosePair);
                                        }
                                        if d.busy {
                                            ui.spinner();
                                        }
                                    });
                                });
                            }
                        });

                    egui::CollapsingHeader::new(semibold(tr("Activity"), 12.5).color(p.text2))
                        .default_open(false)
                        .show(ui, |ui| {
                            if self.audit.is_empty() {
                                ui.label(
                                        RichText::new(tr("No activity yet."))
                                        .size(12.0)
                                        .color(p.text2),
                                );
                            } else {
                                card(ui, |ui| {
                                    for entry in self.audit.iter().rev().take(20) {
                                        ui.label(
                                            RichText::new(entry)
                                                .size(11.5)
                                                .color(p.text2),
                                        );
                                    }
                                });
                            }
                        });

                    }
                    self.devices_focus = false;

                    // The shelf itself.
                    ui.horizontal(|ui| {
                        section_label(ui, "On the shelf");
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if !self.items.is_empty()
                                && capsule(
                                    ui,
                                    "Clear all",
                                    Look::Tint(p.red),
                                    26.0,
                                    None,
                                    true,
                                )
                                .clicked()
                            {
                                actions.push(Action::ClearAll);
                            }
                        });
                    });
                    if self.items.is_empty() {
                        card(ui, |ui| {
                            ui.vertical_centered(|ui| {
                                ui.add_space(6.0);
                                let (r, _) =
                                    ui.allocate_exact_size(vec2(44.0, 44.0), egui::Sense::hover());
                                ui.painter().circle_filled(r.center(), 22.0, p.fill);
                                arrow_tray(ui.painter(), r.center(), p.text3);
                                ui.label(semibold(tr("Nothing on the shelf"), 13.5).color(p.text));
                                ui.label(
                                    RichText::new("Items you receive show up here for an hour.")
                                        .size(12.0)
                                        .color(p.text2),
                                );
                                ui.add_space(6.0);
                            });
                        });
                    }
                    for (i, it) in self.items.iter().enumerate() {
                        let can_remove = it
                            .item
                            .room_id
                            .as_ref()
                            .map(|room_id| {
                                rooms.iter().any(|room| {
                                    &room.id == room_id
                                        && room.owner_public_key == self.device_public_key
                                })
                            })
                            .unwrap_or(true);
                        card(ui, |ui| {
                            ui.horizontal(|ui| {
                                if let Some(preview) = &it.preview {
                                    ui.add(egui::Image::new(egui::load::SizedTexture::new(
                                        preview.id(),
                                        vec2(58.0, 58.0),
                                    )));
                                } else {
                                    let (icon_rect, _) = ui.allocate_exact_size(
                                        vec2(42.0, 42.0),
                                        egui::Sense::hover(),
                                    );
                                    draw_kind_icon(ui.painter(), icon_rect, &it.item.kind);
                                }
                                ui.vertical(|ui| {
                                    ui.spacing_mut().item_spacing.y = 2.0;
                                    ui.label(
                                        semibold(shorten(&it.item.name, 26), 13.5).color(p.text),
                                    );
                                    if let Some(t) = &it.item.text {
                                        ui.label(
                                            RichText::new(shorten(&t.replace('\n', " "), 120))
                                                .size(12.0)
                                                .color(p.text2),
                                        );
                                    }
                                    if it.preview.is_some()
                                        && it.item.path.as_deref().is_some_and(|path| {
                                            path.extension()
                                                .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
                                        })
                                    {
                                        ui.label(
                                            RichText::new("PDF preview")
                                                .size(11.5)
                                                .color(p.text2),
                                        );
                                    }
                                    let mins = ITEM_TTL
                                        .saturating_sub(it.received.elapsed())
                                        .as_secs()
                                        / 60;
                                    ui.label(
                                        RichText::new(format!(
                                            "{} \u{00B7} {} \u{00B7} {} min left",
                                            shorten(&it.item.from, 14),
                                            human_size(it.item.size),
                                            mins
                                        ))
                                        .size(11.5)
                                        .color(p.text3),
                                    );
                                });
                            });
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                if capsule(ui, "Copy", Look::Tint(p.accent), 26.0, None, true)
                                    .clicked()
                                {
                                    actions.push(Action::Copy(i));
                                }
                                if it.item.path.is_some()
                                    && capsule(ui, "Show", Look::Glass, 26.0, None, true).clicked()
                                {
                                    actions.push(Action::Reveal(i));
                                }
                                if can_remove
                                    && capsule(
                                        ui,
                                        "Remove",
                                        Look::Tint(p.red),
                                        26.0,
                                        None,
                                        true,
                                    )
                                    .clicked()
                                    {
                                    actions.push(Action::Remove(i));
                                }
                            });
                        });
                    }
                    ui.add_space(4.0);
                });
            });
        });

        // Apply what the user clicked.
        let mut remove: Vec<usize> = Vec::new();
        for a in actions {
            match a {
                Action::SendClipboard => self.send_clipboard(),
                Action::TogglePhoneUpload => {
                    if self.phone_upload.take().is_some() {
                        self.handle.send(Command::StopWebDrop);
                    } else {
                        self.handle.send(Command::StartWebDrop);
                    }
                }
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
                Action::ClearAll => {
                    remove.extend(0..self.items.len());
                    self.status = "Shelf cleared".to_string();
                }
                Action::CancelTransfer {
                    direction,
                    name,
                    peer,
                } => {
                    self.handle.send(Command::CancelTransfer {
                        direction,
                        name,
                        peer,
                    });
                }
                Action::CreateRoom => {
                    let name = self.room_name_input.trim().to_string();
                    if !name.is_empty() {
                        self.handle.send(Command::CreateRoom { name });
                        self.room_name_input.clear();
                    }
                }
                Action::JoinRoom(addr) => {
                    let room_id = self.room_id_input.trim().to_string();
                    let invite_code = self.room_code_input.trim().to_string();
                    if !room_id.is_empty() && !invite_code.is_empty() {
                        self.handle.send(Command::JoinRoom {
                            addr,
                            room_id,
                            invite_code,
                        });
                        self.set_status("Joining room...");
                    }
                }
                Action::DeleteRoomFile { room_id, hash } => {
                    self.handle.send(Command::DeleteRoomFile { room_id, hash });
                }
                Action::LeaveRoom(room_id) => {
                    self.handle.send(Command::LeaveRoom { room_id });
                }
                Action::DismissRequest(i) => {
                    if i < self.room_requests.len() {
                        self.room_requests.remove(i);
                    }
                }
                Action::FulfillRequest(i) => {
                    if let Some(request) = self.room_requests.get(i) {
                        let room_id = request.room_id.clone();
                        let title = format!("Send \"{}\" to the room", request.name);
                        if let Some(paths) = rfd::FileDialog::new().set_title(&title).pick_files() {
                            self.handle.send(Command::UploadRoomFiles { room_id, paths });
                            self.room_requests.remove(i);
                            self.set_status("Sending requested file to the room...");
                        }
                    }
                }
                Action::DeleteRoom(room_id) => {
                    // Remove it from the screen straight away; the engine confirms (or restores it
                    // if the delete is refused).
                    self.rooms.retain(|room| room.id != room_id);
                    self.room_requests.retain(|request| request.room_id != room_id);
                    if self.active_room_id.as_deref() == Some(room_id.as_str()) {
                        self.active_room_id = None;
                    }
                    self.set_status("Room deleted");
                    self.handle.send(Command::DeleteRoom { room_id });
                }
                Action::RequestRoomFile { room_id } => {
                    let name = self.room_request_input.trim().to_string();
                    if !name.is_empty() {
                        self.handle.send(Command::RequestRoomFile { room_id, name });
                        self.room_request_input.clear();
                        self.set_status("File request sent");
                    }
                }
                Action::SubmitRoomFile { room_id } => {
                    let assignment = self.room_assignment_input.trim().to_string();
                    if assignment.is_empty() {
                        self.set_status("Enter an assignment name first");
                    } else if let Some(paths) = rfd::FileDialog::new()
                        .set_title("Choose files to submit")
                        .pick_files()
                    {
                        if !paths.is_empty() {
                            self.handle.send(Command::SubmitRoomFiles {
                                room_id: room_id.clone(),
                                paths,
                                assignment,
                            });
                            self.set_status("Assignment submission sent");
                            self.active_room_id = Some(room_id);
                        }
                    }
                }
                Action::CreateAssignment { room_id } => {
                    let deadline = self
                        .assignment_deadline_input
                        .trim()
                        .parse::<u64>()
                        .unwrap_or_default();
                    let required_files = self
                        .assignment_files
                        .iter()
                        .filter_map(|path| {
                            path.file_name()
                                .map(|name| name.to_string_lossy().to_string())
                        })
                        .collect::<Vec<_>>();
                    self.handle.send(Command::CreateAssignment {
                        room_id: room_id.clone(),
                        title: self.assignment_title_input.trim().to_string(),
                        deadline,
                        instructions: self.assignment_instructions_input.trim().to_string(),
                        required_files,
                    });
                    if !self.assignment_files.is_empty() {
                        self.handle.send(Command::UploadRoomFiles {
                            room_id,
                            paths: std::mem::take(&mut self.assignment_files),
                        });
                    }
                    self.assignment_title_input.clear();
                    self.assignment_deadline_input.clear();
                    self.assignment_instructions_input.clear();
                }
                Action::ChooseAssignmentFiles => {
                    if let Some(paths) = rfd::FileDialog::new().pick_files() {
                        self.assignment_files = paths;
                    }
                }
                Action::ChooseReceiveFolder => {
                    if let Some(path) = rfd::FileDialog::new()
                        .set_title("Choose download folder")
                        .pick_folder()
                    {
                        self.handle.send(Command::SetReceiveDirectory(path));
                    }
                }
                Action::SaveDeviceGroup => {
                    let members = match &self.target {
                        Target::All => self
                            .paired
                            .iter()
                            .map(|peer| peer.public_key.clone())
                            .collect(),
                        Target::Peer(key) => vec![key.clone()],
                        Target::Peers(keys) => keys.clone(),
                    };
                    self.handle.send(Command::SaveDeviceGroup(DeviceGroup {
                        name: self.group_name_input.trim().to_string(),
                        members,
                    }));
                    self.group_name_input.clear();
                }
                Action::UseDeviceGroup(group) => {
                    self.target = match group.members.len() {
                        0 => Target::All,
                        1 => Target::Peer(group.members[0].clone()),
                        _ => Target::Peers(group.members),
                    };
                    self.set_status(format!("Device group active: {}", group.name));
                }
                Action::ExportRoom(room_id) => {
                    if let Some(path) = rfd::FileDialog::new()
                        .set_title("Export room backup")
                        .set_file_name("shelf-room.zip")
                        .save_file()
                    {
                        self.handle
                            .send(Command::ExportRoomBackup { room_id, path });
                    }
                }
                Action::ImportRoom => {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("Shelf room backup", &["zip"])
                        .pick_file()
                    {
                        self.handle.send(Command::ImportRoomBackup { path });
                    }
                }
                Action::UpdateRoom { room_id, update } => {
                    if matches!(update, RoomUpdate::AddAnnouncement { .. }) {
                        self.room_announcement_input.clear();
                    }
                    if matches!(update, RoomUpdate::AddClipboard { .. }) {
                        self.room_clipboard_input.clear();
                    }
                    if matches!(update, RoomUpdate::AddNote { .. }) {
                        self.room_note_title_input.clear();
                        self.room_note_body_input.clear();
                    }
                    if matches!(update, RoomUpdate::AddLink { .. }) {
                        self.room_link_title_input.clear();
                        self.room_link_url_input.clear();
                    }
                    if matches!(update, RoomUpdate::SetSessionExpiration { .. }) {
                        self.room_expiration_input.clear();
                    }
                    self.handle.send(Command::UpdateRoom { room_id, update });
                }
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

        // Remember the user's preferences between runs.
        let current = Settings {
            side: self.side,
            monitor: self.monitor,
            auto_copy: self.auto_copy,
            pinned: self.pinned,
            material: self.material,
        };
        if current != self.saved_settings {
            current.save();
            self.saved_settings = current;
        }

        // Keeps countdowns and expiry ticking even when nothing else happens.
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

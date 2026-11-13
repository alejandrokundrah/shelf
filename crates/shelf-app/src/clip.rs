//! Windows clipboard helpers. Text and images use `arboard`; file lists go through
//! PowerShell (Get-Clipboard / Set-Clipboard), which is simple and robust.

use anyhow::{bail, Result};
use shelf_core::{ItemKind, Payload, ReceivedItem};
use std::borrow::Cow;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn powershell(script: &str) -> Result<String> {
    let out = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(CREATE_NO_WINDOW)
        .output()?;
    if !out.status.success() {
        bail!("PowerShell failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn clipboard_files() -> Vec<PathBuf> {
    let script = "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; \
                  $f = Get-Clipboard -Format FileDropList; \
                  if ($f) { $f | ForEach-Object { $_.FullName } }";
    match powershell(script) {
        Ok(out) => out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .filter(|p| p.is_file())
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn encode_png(width: usize, height: usize, rgba: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, width as u32, height as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header()?;
        writer.write_image_data(rgba)?;
    }
    Ok(out)
}

/// What is on the clipboard right now: files, else an image, else text.
pub fn read_clipboard() -> Result<Payload> {
    let files = clipboard_files();
    if !files.is_empty() {
        return Ok(Payload::Files(files));
    }
    let mut cb = arboard::Clipboard::new()?;
    if let Ok(img) = cb.get_image() {
        return Ok(Payload::Image(encode_png(img.width, img.height, &img.bytes)?));
    }
    if let Ok(text) = cb.get_text() {
        if !text.trim().is_empty() {
            return Ok(Payload::Text(text));
        }
    }
    bail!("Nothing to send: copy some text, an image or files first")
}

fn copy_files(paths: &[PathBuf]) -> Result<()> {
    let list = paths
        .iter()
        .map(|p| format!("'{}'", p.display().to_string().replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(",");
    powershell(&format!("Set-Clipboard -LiteralPath {list}"))?;
    Ok(())
}

fn copy_image(path: &Path) -> Result<()> {
    let file = std::fs::File::open(path)?;
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder.read_info()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf)?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        bail!("unsupported image format");
    }
    let bytes = buf[..info.buffer_size()].to_vec();
    let mut cb = arboard::Clipboard::new()?;
    cb.set_image(arboard::ImageData {
        width: info.width as usize,
        height: info.height as usize,
        bytes: Cow::Owned(bytes),
    })?;
    Ok(())
}

fn copy_text(text: &str) -> Result<()> {
    arboard::Clipboard::new()?.set_text(text.to_string())?;
    Ok(())
}

/// Puts a received item on the clipboard so a normal Ctrl+V pastes it.
/// Runs on a background thread because PowerShell takes a moment to start.
pub fn copy_item(item: &ReceivedItem) {
    let item = item.clone();
    std::thread::spawn(move || {
        let res = match (item.kind, &item.path, &item.text) {
            (ItemKind::Text, _, Some(t)) => copy_text(t),
            (ItemKind::Image, Some(p), _) => copy_image(p).or_else(|_| copy_files(&[p.clone()])),
            (_, Some(p), _) => copy_files(&[p.clone()]),
            _ => Ok(()),
        };
        if let Err(e) = res {
            eprintln!("clipboard error: {e}");
        }
    });
}

/// Opens Explorer with the file selected.
pub fn reveal(path: &Path) {
    let _ = std::process::Command::new("explorer")
        .raw_arg(format!("/select,\"{}\"", path.display()))
        .spawn();
}

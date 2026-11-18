//! Phone upload ("web drop"): a short-lived HTTP endpoint so a phone that scans a QR code can
//! drop photos and files onto the shelf without installing anything.
//!
//! This is deliberately NOT the paired-device channel. A phone browser cannot do the Noise
//! handshake, so the trade-offs are different and are kept as tight as they can be:
//!
//! * Off by default. It only runs while the user has the QR code showing, and stops by itself
//!   after [`WEBDROP_TTL`] or when the user presses stop.
//! * The only credential is a random 128-bit token inside the URL. A new one is generated every
//!   time the feature is started, so an old QR code stops working.
//! * Plain HTTP on the local network, so it is NOT encrypted: anyone who can watch the traffic
//!   on that network could read the token and the files. The UI says so.
//! * Nothing on the PC can be read through this endpoint except items the user has explicitly
//!   added to the page ("offers"), which are listed by random id. The list is cleared when the
//!   session ends, and no request can name a path.
//!
//! What a phone (or any browser) can send:
//!
//! * `PUT /<token>/<name>`: one file.
//! * `PUT /<token>/folder/<batch>/<root>/<sub>/<file>`, then `POST /<token>/folder/<batch>/done`:
//!   a whole folder. Files are staged in a hidden directory and only appear in the inbox, as one
//!   folder item, when the phone says it is done, so a folder never shows up half-finished.
//! * `POST /<token>/text`: a text note, which arrives like clipboard text from a paired PC.
//!
//! And what the phone can fetch (only items the PC user added to the page):
//!
//! * `GET /<token>/offers`: the list, as JSON.
//! * `GET /<token>/get/<id>`: one offered file, or an offered folder as a zip.

use super::{
    clean_file_name, clear_cancelled, emit_progress, is_cancelled, unique_inbox_path, Event,
    ReceivedItem, Shared, TransferDirection, WebDropOffer,
};
use crate::protocol::{ItemKind, MAX_FILE_BYTES, MAX_TEXT_BYTES};
use crate::store::inbox_dir;
use anyhow::{anyhow, bail, Result};
use rand::Rng;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, Semaphore};
use tokio::time::timeout;

/// How long one phone-upload session stays open.
pub(super) const WEBDROP_TTL: Duration = Duration::from_secs(10 * 60);

/// The label shown in the transfer list and on received items.
const PHONE: &str = "Phone";
const MAX_CONNECTIONS: usize = 8;
const MAX_HEAD_BYTES: usize = 16 * 1024;
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
const BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_NAME_CHARS: usize = 200;
/// Folders being uploaded at the same time in one session.
const MAX_BATCHES: usize = 32;
/// Deepest folder nesting accepted, root included.
const MAX_FOLDER_DEPTH: usize = 32;
/// Most files accepted in one folder upload.
const MAX_FOLDER_FILES: u64 = 20_000;
const PAGE: &str = include_str!("webdrop.html");

/// The running session, kept in `Shared` so it can be stopped from anywhere.
pub(super) struct WebDropSession {
    id: u64,
    stop: Arc<Notify>,
    /// What the PC has put on the page for the phone to download.
    offers: Arc<Mutex<Vec<Offer>>>,
}

/// One item offered to the phone. The path never leaves this struct: the phone only ever sees
/// the id, name and size.
#[derive(Clone)]
struct Offer {
    id: u64,
    name: String,
    path: PathBuf,
    is_dir: bool,
    size: u64,
}

/// Most items the PC can offer at once.
const MAX_OFFERS: usize = 50;
/// Most files and folders packed into one folder download.
const MAX_ZIP_ENTRIES: u64 = 100_000;
const MAX_ZIP_DEPTH: usize = 64;

/// A folder that is still being uploaded. The files themselves live in a hidden staging
/// directory inside the inbox until the phone finishes the batch.
struct Batch {
    root: String,
    files: u64,
}

type Batches = Arc<Mutex<HashMap<String, Batch>>>;

// ---------------------------------------------------------------- session control

/// Opens a fresh session (closing any earlier one) and tells the UI which URL to show.
pub(super) async fn start(shared: &Arc<Shared>) {
    end_session(shared);
    let Some(ip) = local_ipv4() else {
        shared.emit(Event::Info(
            "Phone upload needs a network connection (Wi-Fi or Ethernet).".into(),
        ));
        return;
    };
    let listener = match TcpListener::bind("0.0.0.0:0").await {
        Ok(l) => l,
        Err(e) => {
            shared.emit(Event::Info(format!("Phone upload could not start: {e}")));
            return;
        }
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let token = random_token();
    let id = shared.next_id.fetch_add(1, Ordering::SeqCst);
    let stop = Arc::new(Notify::new());
    *shared.webdrop.lock().unwrap() = Some(WebDropSession {
        id,
        stop: stop.clone(),
        offers: Arc::new(Mutex::new(Vec::new())),
    });
    shared.emit(Event::WebDropReady {
        url: format!("http://{ip}:{port}/{token}"),
        seconds: WEBDROP_TTL.as_secs(),
    });
    tokio::spawn(serve(shared.clone(), listener, token, id, stop, WEBDROP_TTL));
}

/// Stops the current session, if any, and tells the UI.
pub(super) fn stop(shared: &Arc<Shared>) {
    if end_session(shared) {
        shared.emit(Event::WebDropStopped);
    }
}

/// Removes the session and stops its accept loop. Uploads already in flight finish normally.
fn end_session(shared: &Arc<Shared>) -> bool {
    let session = shared.webdrop.lock().unwrap().take();
    match session {
        Some(session) => {
            // `notify_one` keeps a permit, so the signal is not lost if the loop is busy.
            session.stop.notify_one();
            true
        }
        None => false,
    }
}

async fn serve(
    shared: Arc<Shared>,
    listener: TcpListener,
    token: String,
    id: u64,
    stop: Arc<Notify>,
    ttl: Duration,
) {
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let batches: Batches = Arc::new(Mutex::new(HashMap::new()));
    let expiry = tokio::time::sleep(ttl);
    tokio::pin!(expiry);
    loop {
        tokio::select! {
            _ = stop.notified() => break,
            _ = &mut expiry => {
                // Only announce the expiry if this is still the live session.
                stop_session_by_id(&shared, id);
                break;
            }
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let Ok(permit) = slots.clone().try_acquire_owned() else { continue };
                    let (shared, token, batches) = (shared.clone(), token.clone(), batches.clone());
                    tokio::spawn(async move {
                        let _permit = permit;
                        let _ = handle(&shared, stream, &token, &batches).await;
                    });
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
            }
        }
    }
}

fn stop_session_by_id(shared: &Arc<Shared>, id: u64) {
    let mut guard = shared.webdrop.lock().unwrap();
    if guard.as_ref().is_some_and(|s| s.id == id) {
        *guard = None;
        drop(guard);
        shared.emit(Event::WebDropStopped);
    }
}

// ---------------------------------------------------------------- HTTP

struct Request {
    method: String,
    path: String,
    content_length: Option<u64>,
    chunked: bool,
    expect_continue: bool,
    /// Folder uploads: bytes in the whole folder, and bytes of the files already finished.
    /// Only used to draw one steady progress bar for the folder.
    folder_total: Option<u64>,
    folder_done: Option<u64>,
}

async fn handle(
    shared: &Arc<Shared>,
    mut stream: TcpStream,
    token: &str,
    batches: &Batches,
) -> Result<()> {
    let (head, leftover) = timeout(HEAD_TIMEOUT, read_head(&mut stream)).await??;
    let Some(request) = parse_head(&head) else {
        return respond(&mut stream, 400, "text/plain", b"Bad request").await;
    };
    let trimmed = request.path.trim_start_matches('/');
    let (given, rest) = trimmed.split_once('/').unwrap_or((trimmed, ""));
    // Every failure before this point looks identical, so a wrong token learns nothing.
    if !constant_time_eq(given.as_bytes(), token.as_bytes()) {
        return respond(&mut stream, 404, "text/plain", b"Not found").await;
    }
    match (request.method.as_str(), rest) {
        ("GET", "") => {
            let name = shared.config.lock().unwrap().device_name.clone();
            let page = PAGE.replace("{{NAME}}", &html_escape(&name));
            respond(&mut stream, 200, "text/html; charset=utf-8", page.as_bytes()).await
        }
        ("PUT", path) if path.starts_with("folder/") => {
            upload_folder_file(shared, batches, &mut stream, &request, &path[7..], leftover).await
        }
        ("POST", path) if path.starts_with("folder/") => {
            finish_folder(shared, batches, &mut stream, &path[7..]).await
        }
        ("POST", "text") => upload_text(shared, &mut stream, &request, leftover).await,
        ("GET", "offers") => list_offers(shared, &mut stream).await,
        ("GET", path) if path.starts_with("get/") => {
            download(shared, &mut stream, &path[4..]).await
        }
        ("PUT", name) if !name.is_empty() => {
            upload(shared, &mut stream, &request, name, leftover).await
        }
        _ => respond(&mut stream, 404, "text/plain", b"Not found").await,
    }
}

/// Reads up to and including the blank line that ends the request head. Returns the head
/// (without the blank line) and any body bytes that arrived in the same reads.
async fn read_head(stream: &mut TcpStream) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let leftover = buf.split_off(pos + 4);
            buf.truncate(pos);
            return Ok((buf, leftover));
        }
        if buf.len() > MAX_HEAD_BYTES {
            bail!("request header too large");
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            bail!("connection closed before the request finished");
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn parse_head(head: &[u8]) -> Option<Request> {
    let text = std::str::from_utf8(head).ok()?;
    let mut lines = text.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let target = first.next()?;
    let version = first.next()?;
    if !version.starts_with("HTTP/1.") || first.next().is_some() || !target.starts_with('/') {
        return None;
    }
    let path = target.split(['?', '#']).next().unwrap_or("").to_string();
    let mut content_length: Option<u64> = None;
    let mut chunked = false;
    let mut expect_continue = false;
    let mut folder_total: Option<u64> = None;
    let mut folder_done: Option<u64> = None;
    for line in lines {
        let (key, value) = line.split_once(':')?;
        let value = value.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "content-length" => {
                let parsed: u64 = value.parse().ok()?;
                // Two different lengths is a request-smuggling shape. Refuse it.
                if content_length.is_some_and(|existing| existing != parsed) {
                    return None;
                }
                content_length = Some(parsed);
            }
            "transfer-encoding" => chunked = value.to_ascii_lowercase().contains("chunked"),
            "expect" => expect_continue = value.eq_ignore_ascii_case("100-continue"),
            "x-shelf-total" => folder_total = value.parse().ok(),
            "x-shelf-done" => folder_done = value.parse().ok(),
            _ => {}
        }
    }
    Some(Request {
        method,
        path,
        content_length,
        chunked,
        expect_continue,
        folder_total,
        folder_done,
    })
}

async fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) -> Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        411 => "Length Required",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\n\
         Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; \
         script-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; form-action 'none'\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    let _ = stream.shutdown().await;
    Ok(())
}

// ---------------------------------------------------------------- upload

async fn upload(
    shared: &Arc<Shared>,
    stream: &mut TcpStream,
    request: &Request,
    encoded_name: &str,
    leftover: Vec<u8>,
) -> Result<()> {
    if shared.receiving_paused.load(Ordering::SeqCst) {
        return respond(stream, 503, "text/plain", b"Receiving is paused").await;
    }
    let length = match (request.content_length, request.chunked) {
        (Some(length), false) => length,
        _ => return respond(stream, 411, "text/plain", b"Content-Length required").await,
    };
    if length > MAX_FILE_BYTES {
        return respond(stream, 413, "text/plain", b"File too large").await;
    }
    if request.expect_continue {
        stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await?;
    }

    let name = decode_name(encoded_name);
    let base = shared
        .config
        .lock()
        .unwrap()
        .receive_dir
        .clone()
        .unwrap_or_else(inbox_dir);
    tokio::fs::create_dir_all(&base).await?;
    let partial = base.join(format!(".shelf-web-{:016x}.part", rand::random::<u64>()));

    clear_cancelled(shared, TransferDirection::Download, &name, PHONE);
    let received = receive_body(shared, stream, leftover, length, &partial, &name, 0, length).await;
    let hash = match received {
        Ok(hash) => hash,
        Err(error) => {
            let _ = tokio::fs::remove_file(&partial).await;
            clear_cancelled(shared, TransferDirection::Download, &name, PHONE);
            shared.emit(Event::Error(format!("Phone upload of {name} failed: {error}")));
            return respond(stream, 400, "text/plain", b"Upload failed").await;
        }
    };

    if shared.received_hashes.lock().unwrap().contains(&hash) {
        let _ = tokio::fs::remove_file(&partial).await;
        shared.emit(Event::WebDropDuplicate {
            name,
            from: PHONE.to_string(),
        });
        return respond(stream, 200, "application/json", br#"{"status":"duplicate"}"#).await;
    }

    let path = unique_inbox_path(&base, &name);
    if let Err(error) = tokio::fs::rename(&partial, &path).await {
        let _ = tokio::fs::remove_file(&partial).await;
        shared.emit(Event::Error(format!("Could not save {name}: {error}")));
        return respond(stream, 500, "text/plain", b"Could not save the file").await;
    }
    shared.received_hashes.lock().unwrap().insert(hash);
    shared.emit(Event::Received(ReceivedItem {
        id: rand::random::<u64>(),
        room_id: None,
        kind: kind_for(&path),
        name,
        from: PHONE.to_string(),
        size: length,
        path: Some(path),
        text: None,
    }));
    respond(stream, 200, "application/json", br#"{"status":"ok"}"#).await
}

/// Streams exactly `length` body bytes to `partial` and returns their SHA-256.
///
/// Progress is reported as `base + done` out of `total`, so one file of a folder moves the
/// folder's bar instead of getting a row of its own. A single file passes `0` and its length.
#[allow(clippy::too_many_arguments)]
async fn receive_body(
    shared: &Arc<Shared>,
    stream: &mut TcpStream,
    leftover: Vec<u8>,
    length: u64,
    partial: &Path,
    name: &str,
    base: u64,
    total: u64,
) -> Result<[u8; 32]> {
    let mut file = tokio::fs::File::create(partial).await?;
    let mut hasher = Sha256::new();
    let mut done: u64 = 0;
    let mut last_percent = base
        .saturating_mul(100)
        .checked_div(total)
        .unwrap_or(0)
        .min(100) as u8;

    // Body bytes that arrived together with the headers, capped at the declared length.
    let first = &leftover[..(leftover.len() as u64).min(length) as usize];
    if !first.is_empty() {
        file.write_all(first).await?;
        hasher.update(first);
        done += first.len() as u64;
    }

    let mut buf = vec![0u8; 64 * 1024];
    while done < length {
        if is_cancelled(shared, TransferDirection::Download, name, PHONE) {
            bail!("transfer cancelled");
        }
        let want = (length - done).min(buf.len() as u64) as usize;
        let n = timeout(BODY_IDLE_TIMEOUT, stream.read(&mut buf[..want]))
            .await
            .map_err(|_| anyhow!("the phone stopped sending"))??;
        if n == 0 {
            bail!("the phone disconnected before the file finished");
        }
        file.write_all(&buf[..n]).await?;
        hasher.update(&buf[..n]);
        done += n as u64;
        emit_progress(shared, TransferDirection::Download, name, PHONE, base + done, total, &mut last_percent);
    }
    emit_progress(shared, TransferDirection::Download, name, PHONE, base + length, total, &mut last_percent);
    file.flush().await?;
    Ok(hasher.finalize().into())
}

/// Photos get the image preview; everything else is a plain file.
fn kind_for(path: &Path) -> ItemKind {
    let is_image = path.extension().is_some_and(|ext| {
        ["png", "jpg", "jpeg", "gif", "webp", "bmp"]
            .iter()
            .any(|known| ext.eq_ignore_ascii_case(known))
    });
    if is_image {
        ItemKind::Image
    } else {
        ItemKind::File
    }
}

// ---------------------------------------------------------------- folders

/// Splits the part of the URL after `folder/` into the batch id and the folder-relative
/// path segments (root folder first), each one decoded and made safe as a single file name.
fn parse_folder_path(rest: &str) -> Option<(String, Vec<String>)> {
    let (batch, rel) = rest.split_once('/')?;
    let valid_batch = (8..=64).contains(&batch.len()) && batch.bytes().all(|b| b.is_ascii_alphanumeric());
    if !valid_batch {
        return None;
    }
    let raw: Vec<&str> = rel.split('/').collect();
    // A file needs its root folder in front of it, and an empty segment is never legitimate.
    if raw.len() < 2 || raw.len() > MAX_FOLDER_DEPTH || raw.iter().any(|segment| segment.is_empty()) {
        return None;
    }
    let segments = raw.iter().map(|segment| clean_file_name(&decode_name(segment))).collect();
    Some((batch.to_string(), segments))
}

fn staging_dir(base: &Path, batch: &str) -> PathBuf {
    base.join(format!(".shelf-web-dir-{batch}"))
}

async fn receive_dir(shared: &Arc<Shared>) -> Result<PathBuf> {
    let base = shared
        .config
        .lock()
        .unwrap()
        .receive_dir
        .clone()
        .unwrap_or_else(inbox_dir);
    tokio::fs::create_dir_all(&base).await?;
    Ok(base)
}

async fn upload_folder_file(
    shared: &Arc<Shared>,
    batches: &Batches,
    stream: &mut TcpStream,
    request: &Request,
    rest: &str,
    leftover: Vec<u8>,
) -> Result<()> {
    if shared.receiving_paused.load(Ordering::SeqCst) {
        return respond(stream, 503, "text/plain", b"Receiving is paused").await;
    }
    let Some((batch, segments)) = parse_folder_path(rest) else {
        return respond(stream, 400, "text/plain", b"Bad folder path").await;
    };
    let length = match (request.content_length, request.chunked) {
        (Some(length), false) => length,
        _ => return respond(stream, 411, "text/plain", b"Content-Length required").await,
    };
    if length > MAX_FILE_BYTES {
        return respond(stream, 413, "text/plain", b"File too large").await;
    }

    let root = segments[0].clone();
    // Register (or look up) the batch before reading any body. The lock is released before
    // anything is awaited.
    let registered: Result<bool, (u16, &'static [u8])> = {
        let mut guard = batches.lock().unwrap();
        let known = guard.get(&batch).map(|existing| (existing.root.clone(), existing.files));
        match known {
            Some((existing_root, _)) if existing_root != root => {
                Err((400, &b"Folder name changed"[..]))
            }
            Some((_, files)) if files >= MAX_FOLDER_FILES => Err((413, &b"Too many files"[..])),
            Some(_) => Ok(false),
            None if guard.len() >= MAX_BATCHES => {
                Err((503, &b"Too many folders at once"[..]))
            }
            None => {
                guard.insert(batch.clone(), Batch { root: root.clone(), files: 0 });
                Ok(true)
            }
        }
    };
    let first_file = match registered {
        Ok(first) => first,
        Err((status, message)) => return respond(stream, status, "text/plain", message).await,
    };

    if request.expect_continue {
        stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await?;
    }

    let base = receive_dir(shared).await?;
    let staging = staging_dir(&base, &batch);
    let mut target = staging.clone();
    for segment in &segments[1..] {
        target.push(segment);
    }
    let parent = target.parent().unwrap_or(&staging).to_path_buf();
    tokio::fs::create_dir_all(&parent).await?;
    let partial = parent.join(format!(".shelf-web-{:016x}.part", rand::random::<u64>()));

    let base_done = request.folder_done.unwrap_or(0);
    let total = request.folder_total.unwrap_or(length).max(base_done + length).max(1);
    if first_file || base_done == 0 {
        clear_cancelled(shared, TransferDirection::Download, &root, PHONE);
    }
    let received =
        receive_body(shared, stream, leftover, length, &partial, &root, base_done, total).await;
    if let Err(error) = received {
        let _ = tokio::fs::remove_file(&partial).await;
        clear_cancelled(shared, TransferDirection::Download, &root, PHONE);
        shared.emit(Event::Error(format!("Phone upload of {root} failed: {error}")));
        return respond(stream, 400, "text/plain", b"Upload failed").await;
    }
    if let Err(error) = tokio::fs::rename(&partial, &target).await {
        let _ = tokio::fs::remove_file(&partial).await;
        shared.emit(Event::Error(format!("Could not save a file of {root}: {error}")));
        return respond(stream, 500, "text/plain", b"Could not save the file").await;
    }
    if let Some(entry) = batches.lock().unwrap().get_mut(&batch) {
        entry.files += 1;
    }
    respond(stream, 200, "application/json", br#"{"status":"ok"}"#).await
}

/// Counts the files in a staged folder and their total size.
fn folder_stats(dir: &Path) -> (u64, u64) {
    let mut files = 0;
    let mut bytes = 0;
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else { continue };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                pending.push(entry.path());
            } else if !entry.file_name().to_string_lossy().ends_with(".part") {
                files += 1;
                bytes += meta.len();
            }
        }
    }
    (files, bytes)
}

async fn finish_folder(
    shared: &Arc<Shared>,
    batches: &Batches,
    stream: &mut TcpStream,
    rest: &str,
) -> Result<()> {
    let Some(batch) = rest.strip_suffix("/done") else {
        return respond(stream, 404, "text/plain", b"Not found").await;
    };
    let Some(entry) = batches.lock().unwrap().remove(batch) else {
        return respond(stream, 404, "text/plain", b"Unknown folder").await;
    };
    let base = receive_dir(shared).await?;
    let staging = staging_dir(&base, batch);
    let stats_dir = staging.clone();
    let (files, bytes) = tokio::task::spawn_blocking(move || folder_stats(&stats_dir)).await?;
    if files == 0 {
        let _ = tokio::fs::remove_dir_all(&staging).await;
        return respond(stream, 400, "text/plain", b"Nothing was uploaded").await;
    }
    let path = unique_inbox_path(&base, &entry.root);
    if let Err(error) = tokio::fs::rename(&staging, &path).await {
        let _ = tokio::fs::remove_dir_all(&staging).await;
        shared.emit(Event::Error(format!("Could not save folder {}: {error}", entry.root)));
        return respond(stream, 500, "text/plain", b"Could not save the folder").await;
    }
    clear_cancelled(shared, TransferDirection::Download, &entry.root, PHONE);
    shared.emit(Event::Received(ReceivedItem {
        id: rand::random::<u64>(),
        room_id: None,
        kind: ItemKind::File,
        name: entry.root,
        from: PHONE.to_string(),
        size: bytes,
        path: Some(path),
        text: None,
    }));
    respond(stream, 200, "application/json", br#"{"status":"ok"}"#).await
}

// ---------------------------------------------------------------- text

async fn upload_text(
    shared: &Arc<Shared>,
    stream: &mut TcpStream,
    request: &Request,
    leftover: Vec<u8>,
) -> Result<()> {
    if shared.receiving_paused.load(Ordering::SeqCst) {
        return respond(stream, 503, "text/plain", b"Receiving is paused").await;
    }
    let length = match (request.content_length, request.chunked) {
        (Some(length), false) => length,
        _ => return respond(stream, 411, "text/plain", b"Content-Length required").await,
    };
    if length > MAX_TEXT_BYTES {
        return respond(stream, 413, "text/plain", b"Text too large").await;
    }
    if request.expect_continue {
        stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await?;
    }
    let mut body = leftover;
    body.truncate(length as usize);
    while (body.len() as u64) < length {
        let mut chunk = vec![0u8; ((length - body.len() as u64).min(64 * 1024)) as usize];
        let n = match timeout(BODY_IDLE_TIMEOUT, stream.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => n,
            _ => return respond(stream, 400, "text/plain", b"Upload failed").await,
        };
        body.extend_from_slice(&chunk[..n]);
    }
    let Ok(text) = String::from_utf8(body) else {
        return respond(stream, 400, "text/plain", b"Text must be UTF-8").await;
    };
    if text.trim().is_empty() {
        return respond(stream, 400, "text/plain", b"Nothing to send").await;
    }
    let hash: [u8; 32] = Sha256::digest(text.as_bytes()).into();
    if !shared.received_hashes.lock().unwrap().insert(hash) {
        shared.emit(Event::WebDropDuplicate {
            name: "Text".to_string(),
            from: PHONE.to_string(),
        });
        return respond(stream, 200, "application/json", br#"{"status":"duplicate"}"#).await;
    }
    shared.emit(Event::Received(ReceivedItem {
        id: rand::random::<u64>(),
        room_id: None,
        kind: ItemKind::Text,
        name: "Text".to_string(),
        from: PHONE.to_string(),
        size: text.len() as u64,
        path: None,
        text: Some(text),
    }));
    respond(stream, 200, "application/json", br#"{"status":"ok"}"#).await
}

// ---------------------------------------------------------------- PC to phone

fn session_offers(shared: &Arc<Shared>) -> Option<Arc<Mutex<Vec<Offer>>>> {
    shared.webdrop.lock().unwrap().as_ref().map(|session| session.offers.clone())
}

fn offer_infos(offers: &[Offer]) -> Vec<WebDropOffer> {
    offers
        .iter()
        .map(|o| WebDropOffer { id: o.id, name: o.name.clone(), size: o.size, is_dir: o.is_dir })
        .collect()
}

/// Puts files or folders on the phone page. Symbolic links and anything that does not exist
/// are skipped; an item that is already offered is not added twice.
pub(super) async fn offer(shared: &Arc<Shared>, paths: Vec<PathBuf>) {
    let Some(offers) = session_offers(shared) else {
        shared.emit(Event::Info("Open the phone page first, then add files for the phone.".into()));
        return;
    };
    let mut skipped = 0;
    for path in paths {
        let probe = path.clone();
        let measured = tokio::task::spawn_blocking(move || {
            let meta = std::fs::symlink_metadata(&probe).ok()?;
            if meta.file_type().is_symlink() {
                return None;
            }
            let name = probe.file_name()?.to_string_lossy().to_string();
            if meta.is_dir() {
                Some((name, true, folder_stats(&probe).1))
            } else if meta.is_file() {
                Some((name, false, meta.len()))
            } else {
                None
            }
        })
        .await
        .ok()
        .flatten();
        let Some((name, is_dir, size)) = measured else {
            skipped += 1;
            continue;
        };
        let mut guard = offers.lock().unwrap();
        if guard.iter().any(|existing| existing.path == path) {
            continue;
        }
        if guard.len() >= MAX_OFFERS {
            skipped += 1;
            continue;
        }
        let id = rand::random::<u64>();
        guard.push(Offer { id, name, path, is_dir, size });
    }
    if skipped > 0 {
        shared.emit(Event::Info(format!(
            "{skipped} item(s) could not be added for the phone (missing, a link, or the list is full)."
        )));
    }
    let infos = offer_infos(&offers.lock().unwrap());
    shared.emit(Event::WebDropOffers(infos));
}

pub(super) fn withdraw(shared: &Arc<Shared>, id: u64) {
    let Some(offers) = session_offers(shared) else { return };
    let infos = {
        let mut guard = offers.lock().unwrap();
        guard.retain(|o| o.id != id);
        offer_infos(&guard)
    };
    shared.emit(Event::WebDropOffers(infos));
}

fn json_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

async fn list_offers(shared: &Arc<Shared>, stream: &mut TcpStream) -> Result<()> {
    let offers: Vec<Offer> = session_offers(shared)
        .map(|offers| offers.lock().unwrap().clone())
        .unwrap_or_default();
    let items: Vec<String> = offers
        .iter()
        .map(|o| {
            format!(
                r#"{{"id":"{:x}","name":"{}","size":{},"dir":{}}}"#,
                o.id,
                json_escape(&o.name),
                o.size,
                o.is_dir
            )
        })
        .collect();
    let body = format!("[{}]", items.join(","));
    respond(stream, 200, "application/json", body.as_bytes()).await
}

/// Removes a temporary file when dropped, whatever way a download ends.
struct TempFile(PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

async fn download(shared: &Arc<Shared>, stream: &mut TcpStream, id_hex: &str) -> Result<()> {
    let id = match u64::from_str_radix(id_hex, 16) {
        Ok(id) if (1..=16).contains(&id_hex.len()) => id,
        _ => return respond(stream, 404, "text/plain", b"Not found").await,
    };
    let offer = session_offers(shared)
        .and_then(|offers| offers.lock().unwrap().iter().find(|o| o.id == id).cloned());
    let Some(offer) = offer else {
        return respond(stream, 404, "text/plain", b"Not found").await;
    };

    // A folder is packed into a temporary zip first; a file is sent as it is.
    let mut _cleanup = None;
    let (path, file_name) = if offer.is_dir {
        let zip_path = std::env::temp_dir().join(format!("shelf-web-{:016x}.zip", rand::random::<u64>()));
        _cleanup = Some(TempFile(zip_path.clone()));
        let (source, target, root) = (offer.path.clone(), zip_path.clone(), offer.name.clone());
        let packed = tokio::task::spawn_blocking(move || zip_folder(&source, &target, &root)).await;
        match packed {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                shared.emit(Event::Error(format!("Could not pack {} for the phone: {error}", offer.name)));
                return respond(stream, 500, "text/plain", b"Could not pack the folder").await;
            }
            Err(_) => return respond(stream, 500, "text/plain", b"Could not pack the folder").await,
        }
        (zip_path, format!("{}.zip", offer.name))
    } else {
        (offer.path.clone(), offer.name.clone())
    };

    let Ok(mut file) = tokio::fs::File::open(&path).await else {
        return respond(stream, 404, "text/plain", b"Not found").await;
    };
    let Ok(meta) = file.metadata().await else {
        return respond(stream, 404, "text/plain", b"Not found").await;
    };
    if !meta.is_file() {
        return respond(stream, 404, "text/plain", b"Not found").await;
    }
    let length = meta.len();
    let head = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/octet-stream\r\n\
         Content-Length: {length}\r\n\
         Content-Disposition: attachment; filename=\"{}\"; filename*=UTF-8''{}\r\n\
         Cache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\n\
         Content-Security-Policy: default-src 'none'; sandbox\r\n\
         Connection: close\r\n\r\n",
        ascii_file_name(&file_name),
        percent_encode(&file_name),
    );
    timeout(BODY_IDLE_TIMEOUT, stream.write_all(head.as_bytes())).await??;
    let mut buf = vec![0u8; 64 * 1024];
    let mut sent: u64 = 0;
    while sent < length {
        let want = (length - sent).min(buf.len() as u64) as usize;
        let n = file.read(&mut buf[..want]).await?;
        if n == 0 {
            bail!("the file got shorter while it was being sent");
        }
        timeout(BODY_IDLE_TIMEOUT, stream.write_all(&buf[..n]))
            .await
            .map_err(|_| anyhow!("the phone stopped receiving"))??;
        sent += n as u64;
    }
    stream.flush().await?;
    let _ = stream.shutdown().await;
    shared.emit(Event::Info(format!("{} sent to the phone", offer.name)));
    Ok(())
}

/// A header-safe fallback name for old browsers: printable ASCII only.
fn ascii_file_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_graphic() || c == ' ' { c } else { '_' })
        .map(|c| if matches!(c, '"' | '\\' | '%' | ';') { '_' } else { c })
        .collect()
}

/// RFC 5987 percent-encoding for the UTF-8 file name.
fn percent_encode(name: &str) -> String {
    let mut out = String::new();
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Packs `source` into a zip at `dest`, with `root` as the single top-level folder. Links are
/// skipped rather than followed, so a link inside the folder cannot pull in files from elsewhere.
fn zip_folder(source: &Path, dest: &Path, root: &str) -> Result<()> {
    let file = std::fs::File::create(dest)?;
    let mut archive = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let root = root.replace('\\', "/");
    archive.add_directory(format!("{root}/"), options)?;
    let mut entries = 0u64;
    let mut pending: Vec<(PathBuf, String, usize)> = vec![(source.to_path_buf(), root, 1)];
    while let Some((dir, prefix, depth)) = pending.pop() {
        if depth > MAX_ZIP_DEPTH {
            bail!("folders are nested too deeply");
        }
        let mut children: Vec<_> = std::fs::read_dir(&dir)?.flatten().collect();
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            let Ok(kind) = child.file_type() else { continue };
            if kind.is_symlink() {
                continue;
            }
            entries += 1;
            if entries > MAX_ZIP_ENTRIES {
                bail!("the folder has too many files");
            }
            let name = format!("{prefix}/{}", child.file_name().to_string_lossy());
            if kind.is_dir() {
                archive.add_directory(format!("{name}/"), options)?;
                pending.push((child.path(), name, depth + 1));
            } else if kind.is_file() {
                let mut input = std::fs::File::open(child.path())?;
                let big = input.metadata().map(|m| m.len() >= 0xFFFF_FFFF).unwrap_or(false);
                archive.start_file(name, options.large_file(big))?;
                std::io::copy(&mut input, &mut archive)?;
            }
        }
    }
    archive.finish()?.sync_all()?;
    Ok(())
}

// ---------------------------------------------------------------- helpers

fn random_token() -> String {
    let bytes: [u8; 16] = rand::thread_rng().gen();
    hex::encode(bytes)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Decodes the percent-encoded file name from the URL. The result is only a suggestion:
/// `unique_inbox_path` strips any directory part and illegal characters before it is used.
fn decode_name(encoded: &str) -> String {
    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let pair = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(value) = pair.and_then(|p| u8::from_str_radix(p, 16).ok()) {
                out.push(value);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    let decoded = String::from_utf8_lossy(&out);
    let name: String = decoded.chars().take(MAX_NAME_CHARS).collect();
    if name.trim().is_empty() {
        "file".to_string()
    } else {
        name
    }
}

/// The IPv4 address of the interface the PC would use to reach the local network.
///
/// Connecting a UDP socket sends no packets; it only asks the OS which local address it would
/// use. The first probe address is reserved documentation space (it asks "which interface has
/// the default route"). The others cover networks with no internet route, such as a classroom
/// switch or a phone hotspot, which use one of the usual private gateways.
fn local_ipv4() -> Option<Ipv4Addr> {
    let probes = [
        "192.0.2.1:9",
        "192.168.0.1:9",
        "192.168.1.1:9",
        "10.0.0.1:9",
        "172.16.0.1:9",
    ];
    for probe in probes {
        let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") else {
            continue;
        };
        if socket.connect(probe).is_err() {
            continue;
        }
        if let Ok(SocketAddr::V4(local)) = socket.local_addr() {
            let ip = *local.ip();
            if !ip.is_loopback() && !ip.is_unspecified() && !ip.is_link_local() {
                return Some(ip);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Config;
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::sync::mpsc::Receiver;
    use std::sync::Mutex;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "shelf-webdrop-{}-{:08x}",
                std::process::id(),
                rand::random::<u32>()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn test_shared(dir: &Path) -> (Arc<Shared>, Receiver<Event>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let config = Config {
            device_name: "Desk <1>".into(),
            private_key: String::new(),
            protected_key: None,
            public_key: "00".repeat(32),
            peers: Vec::new(),
            rooms: Vec::new(),
            receive_dir: Some(dir.to_path_buf()),
            device_groups: Vec::new(),
        };
        let shared = Arc::new(Shared {
            config: Mutex::new(config),
            events: Mutex::new(tx),
            wake: Box::new(|| {}),
            offer: Mutex::new(None),
            nearby: Mutex::new(HashMap::new()),
            last_known_addrs: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            receiving_paused: AtomicBool::new(false),
            cancelled_transfers: Mutex::new(HashSet::new()),
            received_hashes: Mutex::new(HashSet::new()),
            webdrop: Mutex::new(None),
            stash: Arc::new(crate::stash::Stash::open(std::env::temp_dir().join(format!(
                "shelf-webdrop-stash-{}-{:08x}",
                std::process::id(),
                rand::random::<u32>()
            )))),
        });
        (shared, rx)
    }

    /// Starts the server on a loopback port with a known token.
    async fn serve_for_test(shared: &Arc<Shared>, ttl: Duration) -> (SocketAddr, Arc<Notify>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(Notify::new());
        *shared.webdrop.lock().unwrap() = Some(WebDropSession {
            id: 1,
            stop: stop.clone(),
            offers: Arc::new(Mutex::new(Vec::new())),
        });
        tokio::spawn(serve(shared.clone(), listener, TOKEN.into(), 1, stop.clone(), ttl));
        (addr, stop)
    }

    async fn raw(addr: SocketAddr, head: &str, body: &[u8]) -> (u16, String) {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(head.as_bytes()).await.unwrap();
        for piece in body.chunks(7_919) {
            stream.write_all(piece).await.unwrap();
        }
        let mut out = Vec::new();
        stream.read_to_end(&mut out).await.unwrap();
        let text = String::from_utf8_lossy(&out).to_string();
        let status = text.split(' ').nth(1).unwrap().parse().unwrap();
        (status, text)
    }

    async fn put(addr: SocketAddr, token: &str, name: &str, body: &[u8]) -> (u16, String) {
        let head = format!(
            "PUT /{token}/{name} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        raw(addr, &head, body).await
    }

    fn received(rx: &Receiver<Event>) -> Vec<ReceivedItem> {
        rx.try_iter()
            .filter_map(|event| match event {
                Event::Received(item) => Some(item),
                _ => None,
            })
            .collect()
    }

    fn files_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn page_is_served_only_for_the_right_token() {
        let tmp = TempDir::new();
        let (shared, _rx) = test_shared(&tmp.0);
        let (addr, _stop) = serve_for_test(&shared, Duration::from_secs(60)).await;

        let (status, text) = raw(addr, &format!("GET /{TOKEN} HTTP/1.1\r\nHost: x\r\n\r\n"), b"").await;
        assert_eq!(status, 200);
        assert!(text.contains("Send to Desk &lt;1&gt;"), "device name must be HTML-escaped");
        assert!(!text.contains("{{NAME}}"));
        assert!(text.contains("Cache-Control: no-store"));

        for bad in ["/", "/nope", "/0123456789abcdef0123456789abcdee"] {
            let (status, _) = raw(addr, &format!("GET {bad} HTTP/1.1\r\nHost: x\r\n\r\n"), b"").await;
            assert_eq!(status, 404, "{bad}");
        }
    }

    #[tokio::test]
    async fn upload_lands_in_the_inbox_and_duplicates_are_skipped() {
        let tmp = TempDir::new();
        let (shared, rx) = test_shared(&tmp.0);
        let (addr, _stop) = serve_for_test(&shared, Duration::from_secs(60)).await;

        let (status, text) = put(addr, TOKEN, "hello%20world.txt", b"hi there").await;
        assert_eq!(status, 200);
        assert!(text.contains(r#""status":"ok""#));
        assert_eq!(std::fs::read(tmp.0.join("hello world.txt")).unwrap(), b"hi there");
        let items = received(&rx);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "hello world.txt");
        assert_eq!(items[0].from, "Phone");
        assert_eq!(items[0].size, 8);
        assert_eq!(items[0].kind, ItemKind::File);

        // The same bytes again are recognised and not stored a second time.
        let (status, text) = put(addr, TOKEN, "again.txt", b"hi there").await;
        assert_eq!(status, 200);
        assert!(text.contains("duplicate"));
        assert!(received(&rx).is_empty());
        assert_eq!(files_in(&tmp.0), vec!["hello world.txt"], "no copy and no .part left behind");

        // Same name, different bytes: kept as a numbered copy, never overwritten.
        let (status, _) = put(addr, TOKEN, "hello%20world.txt", b"different").await;
        assert_eq!(status, 200);
        assert_eq!(files_in(&tmp.0), vec!["hello world (1).txt", "hello world.txt"]);
    }

    #[tokio::test]
    async fn photos_get_the_image_kind() {
        let tmp = TempDir::new();
        let (shared, rx) = test_shared(&tmp.0);
        let (addr, _stop) = serve_for_test(&shared, Duration::from_secs(60)).await;
        put(addr, TOKEN, "IMG_0001.JPG", b"not really a jpeg").await;
        put(addr, TOKEN, "scan.heic", b"heic bytes").await;
        let items = received(&rx);
        assert_eq!(items[0].kind, ItemKind::Image);
        assert_eq!(items[1].kind, ItemKind::File);
    }

    #[tokio::test]
    async fn large_upload_is_streamed_intact() {
        let tmp = TempDir::new();
        let (shared, rx) = test_shared(&tmp.0);
        let (addr, _stop) = serve_for_test(&shared, Duration::from_secs(60)).await;
        let body: Vec<u8> = (0..3 * 1024 * 1024u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        let (status, _) = put(addr, TOKEN, "big.bin", &body).await;
        assert_eq!(status, 200);
        assert_eq!(std::fs::read(tmp.0.join("big.bin")).unwrap(), body);
        assert_eq!(received(&rx)[0].size, body.len() as u64);
    }

    #[tokio::test]
    async fn names_cannot_escape_the_inbox() {
        let tmp = TempDir::new();
        let inbox = tmp.0.join("inbox");
        let (shared, _rx) = test_shared(&inbox);
        let (addr, _stop) = serve_for_test(&shared, Duration::from_secs(60)).await;
        for (i, name) in ["..%2F..%2Fevil.txt", "..%5C..%5Cevil2.txt", "%2E%2E"].iter().enumerate() {
            let (status, _) = put(addr, TOKEN, name, format!("payload {i}").as_bytes()).await;
            assert_eq!(status, 200, "{name}");
        }
        assert!(!tmp.0.join("evil.txt").exists());
        assert!(!tmp.0.join("evil2.txt").exists());
        let saved = files_in(&inbox);
        assert_eq!(saved.len(), 3, "{saved:?}");
        assert!(saved.contains(&"evil.txt".to_string()));
    }

    #[tokio::test]
    async fn bad_requests_are_refused_without_touching_disk() {
        let tmp = TempDir::new();
        let (shared, rx) = test_shared(&tmp.0);
        let (addr, _stop) = serve_for_test(&shared, Duration::from_secs(60)).await;

        // Wrong token.
        assert_eq!(put(addr, "wrong", "a.txt", b"x").await.0, 404);
        // No Content-Length.
        let (status, _) = raw(addr, &format!("PUT /{TOKEN}/a.txt HTTP/1.1\r\nHost: x\r\n\r\n"), b"").await;
        assert_eq!(status, 411);
        // Chunked bodies are not supported.
        let head = format!("PUT /{TOKEN}/a.txt HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n");
        assert_eq!(raw(addr, &head, b"").await.0, 411);
        // Declared size over the limit.
        let head = format!(
            "PUT /{TOKEN}/a.txt HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_FILE_BYTES + 1
        );
        assert_eq!(raw(addr, &head, b"").await.0, 413);
        // Conflicting lengths.
        let head = format!("PUT /{TOKEN}/a.txt HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nx");
        assert_eq!(raw(addr, &head, b"").await.0, 400);
        // Garbage.
        assert_eq!(raw(addr, "NOT HTTP\r\n\r\n", b"").await.0, 400);
        // Methods that make no sense here.
        let (status, _) = raw(addr, &format!("DELETE /{TOKEN}/a.txt HTTP/1.1\r\n\r\n"), b"").await;
        assert_eq!(status, 404);
        // Paused receiving.
        shared.receiving_paused.store(true, Ordering::SeqCst);
        assert_eq!(put(addr, TOKEN, "a.txt", b"x").await.0, 503);

        assert!(files_in(&tmp.0).is_empty());
        assert!(received(&rx).is_empty());
    }

    #[tokio::test]
    async fn expect_continue_is_answered() {
        let tmp = TempDir::new();
        let (shared, _rx) = test_shared(&tmp.0);
        let (addr, _stop) = serve_for_test(&shared, Duration::from_secs(60)).await;
        let head = format!(
            "PUT /{TOKEN}/c.txt HTTP/1.1\r\nContent-Length: 3\r\nExpect: 100-continue\r\n\r\n"
        );
        let (_, text) = raw(addr, &head, b"abc").await;
        assert!(text.starts_with("HTTP/1.1 100 Continue"));
        assert!(text.contains("200 OK"));
        assert_eq!(std::fs::read(tmp.0.join("c.txt")).unwrap(), b"abc");
    }

    #[tokio::test]
    async fn a_cut_off_upload_leaves_nothing_behind() {
        let tmp = TempDir::new();
        let (shared, rx) = test_shared(&tmp.0);
        let (addr, _stop) = serve_for_test(&shared, Duration::from_secs(60)).await;

        let mut stream = TcpStream::connect(addr).await.unwrap();
        let head = format!("PUT /{TOKEN}/cut.bin HTTP/1.1\r\nContent-Length: 1000\r\n\r\n");
        stream.write_all(head.as_bytes()).await.unwrap();
        stream.write_all(b"only ten b").await.unwrap();
        drop(stream);

        let mut saw_error = false;
        for _ in 0..40 {
            if rx.try_iter().any(|e| matches!(e, Event::Error(_))) {
                saw_error = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(saw_error, "the failure should be reported");
        assert!(files_in(&tmp.0).is_empty(), "partial file must be removed");
    }

    #[tokio::test]
    async fn cancelling_a_transfer_aborts_the_upload() {
        let tmp = TempDir::new();
        let (shared, rx) = test_shared(&tmp.0);
        let (addr, _stop) = serve_for_test(&shared, Duration::from_secs(60)).await;
        // The flag is cleared when an upload starts, so cancel while it is in flight. The UI
        // cancels by (direction, name, peer).
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let head = format!("PUT /{TOKEN}/slow.bin HTTP/1.1\r\nContent-Length: 200000\r\n\r\n");
        stream.write_all(head.as_bytes()).await.unwrap();
        stream.write_all(&[1u8; 10_000]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        shared
            .cancelled_transfers
            .lock()
            .unwrap()
            .insert(super::super::transfer_key(TransferDirection::Download, "slow.bin", PHONE));
        stream.write_all(&[1u8; 10_000]).await.unwrap();

        let mut out = Vec::new();
        let _ = timeout(Duration::from_secs(3), stream.read_to_end(&mut out)).await;
        assert!(String::from_utf8_lossy(&out).starts_with("HTTP/1.1 400"));
        assert!(rx.try_iter().any(|e| matches!(e, Event::Error(m) if m.contains("cancelled"))));
        assert!(files_in(&tmp.0).is_empty());
    }

    #[tokio::test]
    async fn stopping_closes_the_door() {
        let tmp = TempDir::new();
        let (shared, rx) = test_shared(&tmp.0);
        let (addr, _stop) = serve_for_test(&shared, Duration::from_secs(60)).await;
        assert_eq!(put(addr, TOKEN, "a.txt", b"a").await.0, 200);

        stop(&shared);
        assert!(rx.try_iter().any(|e| matches!(e, Event::WebDropStopped)));
        let mut closed = false;
        for _ in 0..40 {
            if TcpStream::connect(addr).await.is_err() {
                closed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(closed, "new connections must be refused after stop");
        // Stopping twice announces nothing the second time.
        stop(&shared);
        assert!(!rx.try_iter().any(|e| matches!(e, Event::WebDropStopped)));
    }

    #[tokio::test]
    async fn the_session_expires_by_itself() {
        let tmp = TempDir::new();
        let (shared, rx) = test_shared(&tmp.0);
        let (addr, _stop) = serve_for_test(&shared, Duration::from_millis(150)).await;
        let mut stopped = false;
        for _ in 0..40 {
            if rx.try_iter().any(|e| matches!(e, Event::WebDropStopped)) {
                stopped = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(stopped, "expiry should announce the stop");
        assert!(shared.webdrop.lock().unwrap().is_none());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(TcpStream::connect(addr).await.is_err());
    }

    async fn post(addr: SocketAddr, path: &str, body: &[u8]) -> (u16, String) {
        let head = format!(
            "POST /{TOKEN}/{path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        raw(addr, &head, body).await
    }

    fn walk(dir: &Path) -> Vec<String> {
        fn visit(root: &Path, dir: &Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    visit(root, &path, out);
                } else {
                    out.push(path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"));
                }
            }
        }
        let mut out = Vec::new();
        visit(dir, dir, &mut out);
        out.sort();
        out
    }

    const BATCH: &str = "abcdef0123456789";

    #[tokio::test]
    async fn a_folder_arrives_as_one_item_with_its_structure() {
        let tmp = TempDir::new();
        let (shared, rx) = test_shared(&tmp.0);
        let (addr, _stop) = serve_for_test(&shared, Duration::from_secs(60)).await;

        let files: [(&str, &[u8]); 3] = [
            ("Trip%20photos/a.txt", b"alpha"),
            ("Trip%20photos/day%201/b.txt", b"bravo!"),
            ("Trip%20photos/day%201/deep/c.txt", b"charlie"),
        ];
        let total: usize = files.iter().map(|(_, body)| body.len()).sum();
        let mut done = 0;
        for (path, body) in files {
            let head = format!(
                "PUT /{TOKEN}/folder/{BATCH}/{path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\
                 X-Shelf-Total: {total}\r\nX-Shelf-Done: {done}\r\n\r\n",
                body.len()
            );
            let (status, _) = raw(addr, &head, body).await;
            assert_eq!(status, 200, "{path}");
            done += body.len();
        }
        // Nothing is visible in the inbox until the phone says it is finished.
        assert!(received(&rx).is_empty());
        assert!(files_in(&tmp.0).iter().all(|name| name.starts_with('.')), "{:?}", files_in(&tmp.0));

        let (status, _) = post(addr, &format!("folder/{BATCH}/done"), b"").await;
        assert_eq!(status, 200);
        let items = received(&rx);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "Trip photos");
        assert_eq!(items[0].from, "Phone");
        assert_eq!(items[0].size, total as u64);
        assert_eq!(items[0].path.as_deref(), Some(tmp.0.join("Trip photos").as_path()));
        assert_eq!(
            walk(&tmp.0),
            vec!["Trip photos/a.txt", "Trip photos/day 1/b.txt", "Trip photos/day 1/deep/c.txt"]
        );
        assert_eq!(std::fs::read(tmp.0.join("Trip photos/day 1/b.txt")).unwrap(), b"bravo!");
        assert_eq!(files_in(&tmp.0), vec!["Trip photos"], "no staging directory left behind");

        // The batch is closed; finishing it again finds nothing.
        let (status, _) = post(addr, &format!("folder/{BATCH}/done"), b"").await;
        assert_eq!(status, 404);

        // The same folder name again becomes a numbered copy, never a merge.
        let (status, _) = put(addr, TOKEN, &format!("folder/{BATCH}2/Trip%20photos/a.txt"), b"alpha").await;
        assert_eq!(status, 200);
        let (status, _) = post(addr, &format!("folder/{BATCH}2/done"), b"").await;
        assert_eq!(status, 200);
        assert_eq!(files_in(&tmp.0), vec!["Trip photos", "Trip photos (1)"]);
    }

    #[tokio::test]
    async fn folder_paths_cannot_escape_the_inbox() {
        let tmp = TempDir::new();
        let inbox = tmp.0.join("inbox");
        let (shared, _rx) = test_shared(&inbox);
        let (addr, _stop) = serve_for_test(&shared, Duration::from_secs(60)).await;
        for (i, path) in [
            "Root/..%2F..%2Fevil.txt",
            "Root/%2E%2E/evil2.txt",
            "Root/..%5C..%5Cevil3.txt",
            "Root/sub/%2E%2E/%2E%2E/evil4.txt",
        ]
        .iter()
        .enumerate()
        {
            let (status, _) = put(addr, TOKEN, &format!("folder/{BATCH}/{path}"), format!("p{i}").as_bytes()).await;
            assert_eq!(status, 200, "{path}");
        }
        let (status, _) = post(addr, &format!("folder/{BATCH}/done"), b"").await;
        assert_eq!(status, 200);
        assert!(!tmp.0.join("evil.txt").exists());
        assert!(!tmp.0.join("evil2.txt").exists());
        assert!(!tmp.0.join("evil3.txt").exists());
        assert!(!tmp.0.join("evil4.txt").exists());
        let saved = walk(&inbox);
        assert_eq!(saved.len(), 4, "{saved:?}");
        assert!(saved.iter().all(|path| path.starts_with("Root/")), "{saved:?}");
    }

    #[tokio::test]
    async fn bad_folder_requests_are_refused() {
        let tmp = TempDir::new();
        let (shared, rx) = test_shared(&tmp.0);
        let (addr, _stop) = serve_for_test(&shared, Duration::from_secs(60)).await;
        // A file with no folder in front of it, an empty segment, a bad batch id.
        for path in [
            format!("folder/{BATCH}/lonely.txt"),
            format!("folder/{BATCH}/Root//x.txt"),
            "folder/short/Root/x.txt".to_string(),
            "folder/bad!id!bad!id!/Root/x.txt".to_string(),
        ] {
            let (status, _) = put(addr, TOKEN, &path, b"x").await;
            assert_eq!(status, 400, "{path}");
        }
        // One batch cannot switch to a different root folder halfway.
        assert_eq!(put(addr, TOKEN, &format!("folder/{BATCH}/One/a.txt"), b"x").await.0, 200);
        assert_eq!(put(addr, TOKEN, &format!("folder/{BATCH}/Two/b.txt"), b"x").await.0, 400);
        // A wrong token never reaches the folder code.
        let (status, _) = put(addr, "ffffffffffffffffffffffffffffffff", &format!("folder/{BATCH}/One/c.txt"), b"x").await;
        assert_eq!(status, 404);
        // Finishing a batch with no files stores nothing and shows nothing.
        let (status, _) = post(addr, "folder/neverstarted1/done", b"").await;
        assert_eq!(status, 404);
        assert!(received(&rx).is_empty());
    }

    #[tokio::test]
    async fn a_paused_shelf_refuses_folders_and_text() {
        let tmp = TempDir::new();
        let (shared, rx) = test_shared(&tmp.0);
        shared.receiving_paused.store(true, Ordering::SeqCst);
        let (addr, _stop) = serve_for_test(&shared, Duration::from_secs(60)).await;
        assert_eq!(put(addr, TOKEN, &format!("folder/{BATCH}/Root/a.txt"), b"x").await.0, 503);
        assert_eq!(post(addr, "text", b"hello").await.0, 503);
        assert!(received(&rx).is_empty());
        assert!(files_in(&tmp.0).is_empty());
    }

    #[tokio::test]
    async fn text_notes_arrive_like_clipboard_text() {
        let tmp = TempDir::new();
        let (shared, rx) = test_shared(&tmp.0);
        let (addr, _stop) = serve_for_test(&shared, Duration::from_secs(60)).await;

        let note = "Wi-Fi: cafe-guest / pa55w0rd \u{2615}";
        let (status, text) = post(addr, "text", note.as_bytes()).await;
        assert_eq!(status, 200);
        assert!(text.contains(r#""status":"ok""#));
        let items = received(&rx);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, ItemKind::Text);
        assert_eq!(items[0].text.as_deref(), Some(note));
        assert_eq!(items[0].from, "Phone");
        assert_eq!(items[0].size, note.len() as u64);
        assert!(items[0].path.is_none());
        assert!(files_in(&tmp.0).is_empty(), "text is never written to disk");

        // The same note twice is recognised.
        let (status, text) = post(addr, "text", note.as_bytes()).await;
        assert_eq!(status, 200);
        assert!(text.contains("duplicate"));
        assert!(received(&rx).is_empty());

        // Blank, invalid UTF-8 and oversized text are refused.
        assert_eq!(post(addr, "text", b"  \n ").await.0, 400);
        assert_eq!(post(addr, "text", &[0xff, 0xfe, 0xfd]).await.0, 400);
        // Oversized text is refused from the header alone, before any of it is read.
        let head = format!(
            "POST /{TOKEN}/text HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n",
            MAX_TEXT_BYTES + 1
        );
        assert_eq!(raw(addr, &head, b"").await.0, 413);
        assert!(received(&rx).is_empty());
    }

    #[test]
    fn folder_paths_parse_safely() {
        let (batch, segments) = parse_folder_path("abcdef0123/My%20Folder/sub/a%2Fb.txt").unwrap();
        assert_eq!(batch, "abcdef0123");
        assert_eq!(segments, vec!["My Folder", "sub", "b.txt"]);
        // Reserved device names are defused the same way as for single files.
        assert_eq!(parse_folder_path("abcdef0123/Root/NUL.txt").unwrap().1[1], "_NUL.txt");
        assert!(parse_folder_path("abcdef0123/OnlyRoot").is_none());
        assert!(parse_folder_path("abcdef0123").is_none());
        let deep = format!("abcdef0123/{}", vec!["d"; MAX_FOLDER_DEPTH + 1].join("/"));
        assert!(parse_folder_path(&deep).is_none());
    }

    async fn http_get(addr: SocketAddr, token: &str, path: &str) -> (u16, String, Vec<u8>) {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(format!("GET /{token}/{path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut out = Vec::new();
        stream.read_to_end(&mut out).await.unwrap();
        let split = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = String::from_utf8_lossy(&out[..split]).to_string();
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        (status, head, out[split + 4..].to_vec())
    }

    fn last_offers(rx: &Receiver<Event>) -> Option<Vec<WebDropOffer>> {
        rx.try_iter()
            .filter_map(|event| match event {
                Event::WebDropOffers(list) => Some(list),
                _ => None,
            })
            .last()
    }

    async fn offered_list(addr: SocketAddr) -> serde_json::Value {
        let (status, _, body) = http_get(addr, TOKEN, "offers").await;
        assert_eq!(status, 200);
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn only_offered_files_and_folders_can_be_downloaded() {
        let tmp = TempDir::new();
        let (shared, rx) = test_shared(&tmp.0.join("inbox"));
        let (addr, _stop) = serve_for_test(&shared, Duration::from_secs(60)).await;

        // Nothing is offered yet, so the list is empty and nothing can be fetched.
        assert_eq!(offered_list(addr).await, serde_json::json!([]));

        let src = tmp.0.join("src");
        std::fs::create_dir_all(src.join("Album/live")).unwrap();
        let doc = src.join("r\u{e9}sum\u{e9} \"v2\".txt");
        std::fs::write(&doc, b"hello phone").unwrap();
        std::fs::write(src.join("Album/a.txt"), b"alpha").unwrap();
        std::fs::write(src.join("Album/live/b.bin"), vec![7u8; 100_000]).unwrap();
        std::fs::write(src.join("secret.txt"), b"never offered").unwrap();

        offer(&shared, vec![doc.clone(), src.join("Album"), src.join("missing.txt"), doc.clone()]).await;
        let infos = last_offers(&rx).unwrap();
        assert_eq!(infos.len(), 2, "the missing path is skipped and the repeat is not added twice");
        assert!(infos[0].name.starts_with("r\u{e9}sum\u{e9}") && !infos[0].is_dir);
        assert_eq!(infos[0].size, 11);
        assert!(infos[1].is_dir);
        assert_eq!(infos[1].name, "Album");
        assert_eq!(infos[1].size, 100_005);

        let list = offered_list(addr).await;
        assert_eq!(list.as_array().unwrap().len(), 2);
        let file_id = list[0]["id"].as_str().unwrap().to_string();
        let dir_id = list[1]["id"].as_str().unwrap().to_string();
        assert_eq!(list[0]["name"], "r\u{e9}sum\u{e9} \"v2\".txt", "names survive JSON escaping");
        assert_eq!(list[1]["dir"], true);
        assert!(list.to_string().find(&tmp.0.to_string_lossy().to_string()).is_none(), "paths never leave the PC");

        // The file arrives byte for byte, with a safe download name.
        let (status, head, body) = http_get(addr, TOKEN, &format!("get/{file_id}")).await;
        assert_eq!(status, 200);
        assert_eq!(body, b"hello phone");
        assert!(head.contains("Content-Length: 11"), "{head}");
        assert!(head.contains("attachment; filename=\"r_sum_ _v2_.txt\""), "{head}");
        assert!(head.contains("filename*=UTF-8''r%C3%A9sum%C3%A9%20%22v2%22.txt"), "{head}");

        // The folder arrives as a zip with its structure under one top-level folder.
        let (status, head, body) = http_get(addr, TOKEN, &format!("get/{dir_id}")).await;
        assert_eq!(status, 200);
        assert!(head.contains("filename=\"Album.zip\""), "{head}");
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(body)).unwrap();
        let mut names: Vec<String> = (0..archive.len()).map(|i| archive.by_index(i).unwrap().name().to_string()).collect();
        names.sort();
        assert_eq!(names, vec!["Album/", "Album/a.txt", "Album/live/", "Album/live/b.bin"]);
        let mut inner = String::new();
        std::io::Read::read_to_string(&mut archive.by_name("Album/a.txt").unwrap(), &mut inner).unwrap();
        assert_eq!(inner, "alpha");

        // Everything else is unreachable: unknown ids, odd ids, paths, wrong token.
        for path in ["get/1", "get/ffffffffffffffff", "get/zz", "get/", "get/..%2Fsecret.txt", "get/../secret.txt", "get/00000000000000000"] {
            let (status, _, _) = http_get(addr, TOKEN, path).await;
            assert_eq!(status, 404, "{path}");
        }
        let (status, _, _) = http_get(addr, "ffffffffffffffffffffffffffffffff", &format!("get/{file_id}")).await;
        assert_eq!(status, 404);
        let (status, _, _) = http_get(addr, "ffffffffffffffffffffffffffffffff", "offers").await;
        assert_eq!(status, 404);

        // Taking an item off the page makes it unreachable straight away.
        let withdrawn = infos[0].id;
        withdraw(&shared, withdrawn);
        assert_eq!(last_offers(&rx).unwrap().len(), 1);
        let (status, _, _) = http_get(addr, TOKEN, &format!("get/{file_id}")).await;
        assert_eq!(status, 404);
        assert_eq!(offered_list(addr).await.as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn offers_go_away_with_the_session_and_need_one() {
        let tmp = TempDir::new();
        let (shared, rx) = test_shared(&tmp.0.join("inbox"));
        let file = tmp.0.join("note.txt");
        std::fs::write(&file, b"x").unwrap();

        // No page open: nothing is offered, and the user is told why.
        offer(&shared, vec![file.clone()]).await;
        assert!(last_offers(&rx).is_none());

        let (addr, stop) = serve_for_test(&shared, Duration::from_secs(60)).await;
        offer(&shared, vec![file]).await;
        let id = format!("{:x}", last_offers(&rx).unwrap()[0].id);
        assert_eq!(http_get(addr, TOKEN, &format!("get/{id}")).await.0, 200);

        stop_session_by_id(&shared, 1);
        stop.notify_one();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(session_offers(&shared).is_none(), "the list is gone with the session");
        assert!(TcpStream::connect(addr).await.is_err(), "and so is the door");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn links_are_not_offered_or_followed() {
        let tmp = TempDir::new();
        let (shared, rx) = test_shared(&tmp.0.join("inbox"));
        let (addr, _stop) = serve_for_test(&shared, Duration::from_secs(60)).await;
        let outside = tmp.0.join("outside.txt");
        std::fs::write(&outside, b"private").unwrap();
        let folder = tmp.0.join("Docs");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("ok.txt"), b"ok").unwrap();
        std::os::unix::fs::symlink(&outside, folder.join("link.txt")).unwrap();
        std::os::unix::fs::symlink(&outside, tmp.0.join("direct-link.txt")).unwrap();

        offer(&shared, vec![tmp.0.join("direct-link.txt"), folder]).await;
        let infos = last_offers(&rx).unwrap();
        assert_eq!(infos.len(), 1, "a link cannot be offered directly");
        let (status, _, body) = http_get(addr, TOKEN, &format!("get/{:x}", infos[0].id)).await;
        assert_eq!(status, 200);
        let archive = zip::ZipArchive::new(std::io::Cursor::new(body)).unwrap();
        let names: Vec<&str> = archive.file_names().collect();
        assert!(names.contains(&"Docs/ok.txt") && !names.iter().any(|n| n.contains("link")), "{names:?}");
    }

    #[test]
    fn download_name_helpers() {
        assert_eq!(json_escape("a\"b\\c\n\u{1}"), "a\\\"b\\\\c\\n\\u0001".replace("\\n", "\\u000a"));
        assert_eq!(ascii_file_name("caf\u{e9} \"x\"; 50%.txt"), "caf_ _x__ 50_.txt");
        assert_eq!(percent_encode("a b/\u{e9}.txt"), "a%20b%2F%C3%A9.txt");
    }

    #[test]
    fn names_decode_safely() {
        assert_eq!(decode_name("a%20b.txt"), "a b.txt");
        assert_eq!(decode_name("%E2%9C%93.txt"), "✓.txt");
        assert_eq!(decode_name("100%.txt"), "100%.txt");
        assert_eq!(decode_name("bad%zzname"), "bad%zzname");
        assert_eq!(decode_name("end%2"), "end%2");
        assert_eq!(decode_name("   "), "file");
        assert_eq!(decode_name(""), "file");
        assert_eq!(decode_name(&"x".repeat(500)).chars().count(), MAX_NAME_CHARS);
    }

    #[test]
    fn head_parsing_and_helpers() {
        let ok = parse_head(b"PUT /t/f%20x?y=1 HTTP/1.1\r\ncontent-length: 5\r\nExpect: 100-Continue").unwrap();
        assert_eq!((ok.method.as_str(), ok.path.as_str()), ("PUT", "/t/f%20x"));
        assert_eq!(ok.content_length, Some(5));
        assert!(ok.expect_continue && !ok.chunked);
        assert!(parse_head(b"GET / HTTP/2.0").is_none());
        assert!(parse_head(b"GET nope HTTP/1.1").is_none());
        assert!(parse_head(b"GET / HTTP/1.1\r\nContent-Length: -1").is_none());
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert_eq!(html_escape("<a href=\"x\">&'"), "&lt;a href=&quot;x&quot;&gt;&amp;&#39;");
        assert_eq!(random_token().len(), 32);
        assert_ne!(random_token(), random_token());
    }
}

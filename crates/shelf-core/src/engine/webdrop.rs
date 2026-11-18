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
//! * Upload only. Nothing on the shelf can be read back through this endpoint.

use super::{
    emit_progress, is_cancelled, clear_cancelled, unique_inbox_path, Event, ReceivedItem, Shared,
    TransferDirection,
};
use crate::protocol::{ItemKind, MAX_FILE_BYTES};
use crate::store::inbox_dir;
use anyhow::{anyhow, bail, Result};
use rand::Rng;
use sha2::{Digest, Sha256};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
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
const PAGE: &str = include_str!("webdrop.html");

/// The running session, kept in `Shared` so it can be stopped from anywhere.
pub(super) struct WebDropSession {
    id: u64,
    stop: Arc<Notify>,
}

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
                    let (shared, token) = (shared.clone(), token.clone());
                    tokio::spawn(async move {
                        let _permit = permit;
                        let _ = handle(&shared, stream, &token).await;
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
}

async fn handle(shared: &Arc<Shared>, mut stream: TcpStream, token: &str) -> Result<()> {
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
            _ => {}
        }
    }
    Some(Request {
        method,
        path,
        content_length,
        chunked,
        expect_continue,
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
    let received = receive_body(shared, stream, leftover, length, &partial, &name).await;
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
async fn receive_body(
    shared: &Arc<Shared>,
    stream: &mut TcpStream,
    leftover: Vec<u8>,
    length: u64,
    partial: &Path,
    name: &str,
) -> Result<[u8; 32]> {
    let mut file = tokio::fs::File::create(partial).await?;
    let mut hasher = Sha256::new();
    let mut done: u64 = 0;
    let mut last_percent = 0u8;

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
        emit_progress(shared, TransferDirection::Download, name, PHONE, done, length, &mut last_percent);
    }
    emit_progress(shared, TransferDirection::Download, name, PHONE, length, length, &mut last_percent);
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

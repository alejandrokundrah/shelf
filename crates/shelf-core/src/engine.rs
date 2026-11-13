//! The engine runs on its own thread with a tokio runtime. The UI sends `Command`s
//! and receives `Event`s; `wake` is called after every event so the UI can repaint.

use crate::channel::SecureChannel;
use crate::pairing;
use crate::protocol::{ItemHeader, ItemKind, CHUNK, MAX_FILE_BYTES, MAX_TEXT_BYTES};
use crate::store::{inbox_dir, Config, PairedPeer};
use anyhow::{anyhow, bail, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use rand::Rng;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::time::timeout;

const SERVICE_TYPE: &str = "_shelf._tcp.local.";
const PAIR_TTL: Duration = Duration::from_secs(120);
/// How often byte counts are reported for a running transfer.
const PROGRESS_EVERY: Duration = Duration::from_millis(120);

// ---------------------------------------------------------------- public types

#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    All,
    Peer(String), // public key (hex)
}

#[derive(Clone, Debug)]
pub enum Payload {
    Files(Vec<PathBuf>),
    Text(String),
    Image(Vec<u8>), // PNG bytes
}

pub enum Command {
    StartPairing,
    CancelPairing,
    PairWith { addr: SocketAddr, code: String },
    Unpair { public_key: String },
    Send { target: Target, payload: Payload },
    Shutdown,
}

#[derive(Clone, Debug)]
pub struct PeerStatus {
    pub name: String,
    pub public_key: String,
    /// Some(..) when the device is currently visible on the network.
    pub addr: Option<SocketAddr>,
}

#[derive(Clone, Debug)]
pub struct NearbyDevice {
    pub name: String,
    pub public_key: String,
    pub addr: SocketAddr,
}

#[derive(Clone, Debug)]
pub struct ReceivedItem {
    pub id: u64,
    pub kind: ItemKind,
    pub name: String,
    pub from: String,
    pub size: u64,
    pub path: Option<PathBuf>,
    pub text: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferDir {
    Upload,
    Download,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TransferState {
    /// Waiting for earlier items to finish.
    Queued,
    Active,
    Done,
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct TransferUpdate {
    pub id: u64,
    pub dir: TransferDir,
    pub name: String,
    /// The other device.
    pub peer: String,
    pub bytes: u64,
    pub total: u64,
    pub state: TransferState,
}

pub enum Event {
    Ready { device_name: String, public_key: String },
    Peers { paired: Vec<PeerStatus>, nearby: Vec<NearbyDevice> },
    PairingCode { code: String, seconds: u64 },
    PairingCleared,
    PairingResult(Result<String, String>),
    Received(ReceivedItem),
    /// Progress of one item being sent or received. The UI derives speed and time left.
    Transfer(TransferUpdate),
    Info(String),
    Error(String),
}

#[derive(Clone)]
pub struct Handle {
    tx: UnboundedSender<Command>,
}

impl Handle {
    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
    }
}

// ---------------------------------------------------------------- shared state

struct PairOffer {
    code: String,
    expires: Instant,
    id: u64,
}

struct Dest {
    pk: String,
    name: String,
    addr: SocketAddr,
}

struct Shared {
    config: Mutex<Config>,
    events: Mutex<Sender<Event>>,
    wake: Box<dyn Fn() + Send + Sync>,
    offer: Mutex<Option<PairOffer>>,
    nearby: Mutex<HashMap<String, NearbyDevice>>,
    next_id: AtomicU64,
}

impl Shared {
    fn emit(&self, e: Event) {
        if let Ok(tx) = self.events.lock() {
            let _ = tx.send(e);
        }
        (self.wake)();
    }

    fn publish_peers(&self) {
        let peers = self.config.lock().unwrap().peers.clone();
        let (paired, nearby_list) = {
            let nearby = self.nearby.lock().unwrap();
            let paired: Vec<PeerStatus> = peers
                .iter()
                .map(|p| PeerStatus {
                    name: p.name.clone(),
                    public_key: p.public_key.clone(),
                    addr: nearby
                        .values()
                        .find(|d| d.public_key == p.public_key)
                        .map(|d| d.addr),
                })
                .collect();
            let unpaired: Vec<NearbyDevice> = nearby
                .values()
                .filter(|d| !peers.iter().any(|p| p.public_key == d.public_key))
                .cloned()
                .collect();
            (paired, unpaired)
        };
        self.emit(Event::Peers { paired, nearby: nearby_list });
    }

    fn add_peer(&self, peer: PairedPeer) {
        let res = {
            let mut c = self.config.lock().unwrap();
            c.peers.retain(|p| p.public_key != peer.public_key);
            c.peers.push(peer);
            c.save()
        };
        if let Err(e) = res {
            self.emit(Event::Error(format!("could not save pairing: {e}")));
        }
        self.publish_peers();
    }

    fn remove_peer(&self, public_key: &str) {
        let res = {
            let mut c = self.config.lock().unwrap();
            c.peers.retain(|p| p.public_key != public_key);
            c.save()
        };
        if let Err(e) = res {
            self.emit(Event::Error(format!("could not save config: {e}")));
        }
        self.publish_peers();
    }

    fn targets(&self, t: &Target) -> Vec<Dest> {
        let cfg = self.config.lock().unwrap();
        let nearby = self.nearby.lock().unwrap();
        cfg.peers
            .iter()
            .filter(|p| match t {
                Target::All => true,
                Target::Peer(k) => &p.public_key == k,
            })
            .filter_map(|p| {
                nearby
                    .values()
                    .find(|d| d.public_key == p.public_key)
                    .map(|d| Dest {
                        pk: p.public_key.clone(),
                        name: p.name.clone(),
                        addr: d.addr,
                    })
            })
            .collect()
    }

    fn private_key(&self) -> Result<Vec<u8>> {
        let hex_key = self.config.lock().unwrap().private_key.clone();
        Ok(hex::decode(hex_key)?)
    }

    fn new_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    #[allow(clippy::too_many_arguments)]
    fn transfer(
        &self,
        id: u64,
        dir: TransferDir,
        name: &str,
        peer: &str,
        bytes: u64,
        total: u64,
        state: TransferState,
    ) {
        self.emit(Event::Transfer(TransferUpdate {
            id,
            dir,
            name: name.to_string(),
            peer: peer.to_string(),
            bytes,
            total,
            state,
        }));
    }

    fn finish_send(&self, id: u64, name: &str, peer: &str, total: u64, res: Result<()>) {
        let state = match res {
            Ok(()) => TransferState::Done,
            Err(e) => TransferState::Failed(e.to_string()),
        };
        let bytes = if state == TransferState::Done { total } else { 0 };
        self.transfer(id, TransferDir::Upload, name, peer, bytes, total, state);
    }
}

// ---------------------------------------------------------------- startup

pub fn start(wake: impl Fn() + Send + Sync + 'static) -> Result<(Handle, Receiver<Event>)> {
    let config = Config::load_or_create()?;
    let (ev_tx, ev_rx) = std::sync::mpsc::channel();
    let (cmd_tx, cmd_rx) = unbounded_channel();
    let shared = Arc::new(Shared {
        config: Mutex::new(config),
        events: Mutex::new(ev_tx),
        wake: Box::new(wake),
        offer: Mutex::new(None),
        nearby: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
    });
    std::thread::Builder::new()
        .name("shelf-engine".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(run(shared, cmd_rx));
        })?;
    Ok((Handle { tx: cmd_tx }, ev_rx))
}

fn sanitize(name: &str) -> String {
    name.chars()
        .take(40)
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn start_discovery(
    shared: &Arc<Shared>,
    name: &str,
    public_key: &str,
    port: u16,
) -> Result<(ServiceDaemon, String)> {
    let daemon = ServiceDaemon::new()?;
    let instance = format!("{}-{}", sanitize(name), &public_key[..6]);
    let host = format!("{instance}.local.");
    let mut props = HashMap::new();
    props.insert("v".to_string(), "1".to_string());
    props.insert("pk".to_string(), public_key.to_string());
    props.insert("name".to_string(), name.to_string());
    let info = ServiceInfo::new(SERVICE_TYPE, &instance, &host, "", port, props)?.enable_addr_auto();
    let fullname = info.get_fullname().to_string();
    daemon.register(info)?;

    let rx = daemon.browse(SERVICE_TYPE)?;
    let shared = shared.clone();
    let my_key = public_key.to_string();
    std::thread::spawn(move || loop {
        let started = Instant::now();
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                let pk = info.get_property_val_str("pk").unwrap_or("").to_string();
                if pk.len() != 64 || pk == my_key {
                    continue;
                }
                let dname = info.get_property_val_str("name").unwrap_or("Unknown").to_string();
                let ip = info.get_addresses().iter().find(|a| a.is_ipv4()).copied();
                if let Some(ip) = ip {
                    let dev = NearbyDevice {
                        name: dname,
                        public_key: pk,
                        addr: SocketAddr::new(ip, info.get_port()),
                    };
                    shared
                        .nearby
                        .lock()
                        .unwrap()
                        .insert(info.get_fullname().to_string(), dev);
                    shared.publish_peers();
                }
            }
            Ok(ServiceEvent::ServiceRemoved(_, fullname)) => {
                shared.nearby.lock().unwrap().remove(&fullname);
                shared.publish_peers();
            }
            Ok(_) => {}
            Err(_) => {
                // A timeout takes ~1s; returning instantly means the daemon shut down.
                if started.elapsed() < Duration::from_millis(500) {
                    break;
                }
            }
        }
    });
    Ok((daemon, fullname))
}

async fn run(shared: Arc<Shared>, mut cmd_rx: UnboundedReceiver<Command>) {
    let listener = match TcpListener::bind("0.0.0.0:0").await {
        Ok(l) => l,
        Err(e) => {
            shared.emit(Event::Error(format!("cannot listen for connections: {e}")));
            return;
        }
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let (name, public_key) = {
        let c = shared.config.lock().unwrap();
        (c.device_name.clone(), c.public_key.clone())
    };
    shared.emit(Event::Ready {
        device_name: name.clone(),
        public_key: public_key.clone(),
    });

    let daemon = match start_discovery(&shared, &name, &public_key, port) {
        Ok(d) => Some(d),
        Err(e) => {
            shared.emit(Event::Error(format!("device discovery unavailable: {e}")));
            None
        }
    };
    shared.publish_peers();

    let accept_shared = shared.clone();
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let s = accept_shared.clone();
                    tokio::spawn(async move { handle_conn(s, stream).await });
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
            }
        }
    });

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            Command::Shutdown => break,
            Command::StartPairing => {
                let code = format!("{:06}", rand::thread_rng().gen_range(0..1_000_000u32));
                let id = shared.next_id.fetch_add(1, Ordering::SeqCst);
                *shared.offer.lock().unwrap() = Some(PairOffer {
                    code: code.clone(),
                    expires: Instant::now() + PAIR_TTL,
                    id,
                });
                shared.emit(Event::PairingCode { code, seconds: PAIR_TTL.as_secs() });
                let s = shared.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(PAIR_TTL).await;
                    let cleared = {
                        let mut o = s.offer.lock().unwrap();
                        if o.as_ref().map(|x| x.id) == Some(id) {
                            *o = None;
                            true
                        } else {
                            false
                        }
                    };
                    if cleared {
                        s.emit(Event::PairingCleared);
                    }
                });
            }
            Command::CancelPairing => {
                *shared.offer.lock().unwrap() = None;
                shared.emit(Event::PairingCleared);
            }
            Command::PairWith { addr, code } => {
                let s = shared.clone();
                tokio::spawn(async move {
                    match pair_with(&s, addr, &code).await {
                        Ok(peer) => {
                            let name = peer.name.clone();
                            s.add_peer(peer);
                            s.emit(Event::PairingResult(Ok(name)));
                        }
                        Err(e) => s.emit(Event::PairingResult(Err(e.to_string()))),
                    }
                });
            }
            Command::Unpair { public_key } => shared.remove_peer(&public_key),
            Command::Send { target, payload } => {
                let targets = shared.targets(&target);
                if targets.is_empty() {
                    shared.emit(Event::Error("No paired device is nearby right now".into()));
                    continue;
                }
                let s = shared.clone();
                tokio::spawn(send_payload(s, targets, payload));
            }
        }
    }

    if let Some((daemon, fullname)) = daemon {
        let _ = daemon.unregister(&fullname);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let _ = daemon.shutdown();
    }
}

// ---------------------------------------------------------------- pairing

async fn pair_with(shared: &Arc<Shared>, addr: SocketAddr, code: &str) -> Result<PairedPeer> {
    let me = shared.config.lock().unwrap().clone();
    let mut stream = timeout(Duration::from_secs(5), TcpStream::connect(addr)).await??;
    stream.write_u8(b'P').await?;
    timeout(Duration::from_secs(30), pairing::run(stream, code, &me)).await?
}

async fn handle_pairing(shared: &Arc<Shared>, stream: TcpStream) -> Result<()> {
    // One attempt per displayed code: the offer is burned as soon as someone tries it.
    let offer = shared.offer.lock().unwrap().take();
    let Some(offer) = offer else {
        bail!("someone tried to pair while pairing was off");
    };
    shared.emit(Event::PairingCleared);
    if Instant::now() > offer.expires {
        bail!("pairing code had expired");
    }
    let me = shared.config.lock().unwrap().clone();
    match timeout(Duration::from_secs(30), pairing::run(stream, &offer.code, &me)).await {
        Ok(Ok(peer)) => {
            let name = peer.name.clone();
            shared.add_peer(peer);
            shared.emit(Event::PairingResult(Ok(name)));
        }
        Ok(Err(e)) => shared.emit(Event::PairingResult(Err(e.to_string()))),
        Err(_) => shared.emit(Event::PairingResult(Err("pairing timed out".into()))),
    }
    Ok(())
}

// ---------------------------------------------------------------- receiving

async fn handle_conn(shared: Arc<Shared>, mut stream: TcpStream) {
    let tag = match timeout(Duration::from_secs(5), stream.read_u8()).await {
        Ok(Ok(t)) => t,
        _ => return,
    };
    let result = match tag {
        b'P' => handle_pairing(&shared, stream).await,
        b'T' => handle_transfer(&shared, stream).await,
        _ => Ok(()),
    };
    if let Err(e) = result {
        shared.emit(Event::Error(e.to_string()));
    }
}

const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

fn unique_inbox_path(inbox: &Path, name: &str) -> PathBuf {
    let base = Path::new(name)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut clean: String = base
        .chars()
        .map(|c| if "<>:\"/\\|?*".contains(c) || c.is_control() { '_' } else { c })
        .collect();
    clean = clean.trim_matches(|c| c == ' ' || c == '.').to_string();
    if clean.is_empty() {
        clean = "file".to_string();
    }
    let stem_upper = clean.split('.').next().unwrap_or("").to_uppercase();
    if RESERVED.contains(&stem_upper.as_str()) {
        clean = format!("_{clean}");
    }

    let first = inbox.join(&clean);
    if !first.exists() {
        return first;
    }
    let p = Path::new(&clean);
    let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let mut n = 1;
    loop {
        let candidate = inbox.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

async fn receive_to_file(
    ch: &mut SecureChannel,
    path: &Path,
    size: u64,
    on_progress: &mut (dyn FnMut(u64) + Send),
) -> Result<()> {
    let mut f = tokio::fs::File::create(path).await?;
    let mut left = size;
    while left > 0 {
        let chunk = ch.recv().await?;
        if chunk.is_empty() || chunk.len() as u64 > left {
            bail!("corrupt transfer");
        }
        f.write_all(&chunk).await?;
        left -= chunk.len() as u64;
        on_progress(size - left);
    }
    f.flush().await?;
    Ok(())
}

async fn handle_transfer(shared: &Arc<Shared>, stream: TcpStream) -> Result<()> {
    let key = shared.private_key()?;
    let mut ch = timeout(Duration::from_secs(10), SecureChannel::accept(stream, &key)).await??;

    let peer = {
        let c = shared.config.lock().unwrap();
        c.find_peer(&ch.remote_static).cloned()
    };
    let peer = peer.ok_or_else(|| anyhow!("rejected a connection from an unpaired device"))?;

    let header: ItemHeader = serde_json::from_slice(&ch.recv().await?)?;
    if header.name.len() > 512 {
        bail!("item name too long");
    }
    let limit = if header.kind == ItemKind::Text { MAX_TEXT_BYTES } else { MAX_FILE_BYTES };
    let id = shared.new_id();
    let (name, size) = (header.name.clone(), header.size);
    if header.size > limit {
        let msg = "too large to receive".to_string();
        shared.transfer(id, TransferDir::Download, &name, &peer.name, 0, size, TransferState::Failed(msg));
        return Ok(());
    }
    shared.transfer(id, TransferDir::Download, &name, &peer.name, 0, size, TransferState::Active);

    let result: Result<(Option<PathBuf>, Option<String>)> = async {
        if header.kind == ItemKind::Text {
            let mut buf = Vec::with_capacity(header.size as usize);
            let mut left = header.size;
            while left > 0 {
                let chunk = ch.recv().await?;
                if chunk.is_empty() || chunk.len() as u64 > left {
                    bail!("corrupt transfer");
                }
                left -= chunk.len() as u64;
                buf.extend_from_slice(&chunk);
            }
            let text = String::from_utf8(buf).map_err(|_| anyhow!("text was not valid UTF-8"))?;
            Ok((None, Some(text)))
        } else {
            let inbox = inbox_dir();
            tokio::fs::create_dir_all(&inbox).await?;
            let path = unique_inbox_path(&inbox, &header.name);
            let mut last = Instant::now();
            let sh = shared.clone();
            let (n, from) = (name.clone(), peer.name.clone());
            let mut on_progress = move |done: u64| {
                if last.elapsed() >= PROGRESS_EVERY {
                    last = Instant::now();
                    sh.transfer(id, TransferDir::Download, &n, &from, done, size, TransferState::Active);
                }
            };
            if let Err(e) = receive_to_file(&mut ch, &path, header.size, &mut on_progress).await {
                let _ = tokio::fs::remove_file(&path).await;
                return Err(e);
            }
            Ok((Some(path), None))
        }
    }
    .await;

    let (path, text) = match result {
        Ok(v) => v,
        Err(e) => {
            let state = TransferState::Failed(e.to_string());
            shared.transfer(id, TransferDir::Download, &name, &peer.name, 0, size, state);
            return Ok(());
        }
    };

    ch.send(b"OK").await?;
    shared.transfer(id, TransferDir::Download, &name, &peer.name, size, size, TransferState::Done);
    shared.emit(Event::Received(ReceivedItem {
        id: rand::random::<u64>(),
        kind: header.kind,
        name: header.name,
        from: peer.name,
        size: header.size,
        path,
        text,
    }));
    Ok(())
}

// ---------------------------------------------------------------- sending

async fn push_item<R: AsyncRead + Unpin>(
    shared: &Arc<Shared>,
    dest: &Dest,
    header: ItemHeader,
    mut reader: R,
    label: &str,
    id: u64,
) -> Result<()> {
    let total = header.size;
    shared.transfer(id, TransferDir::Upload, label, &dest.name, 0, total, TransferState::Active);
    let key = shared.private_key()?;
    let mut stream = timeout(Duration::from_secs(5), TcpStream::connect(dest.addr)).await??;
    stream.write_u8(b'T').await?;
    let mut ch = timeout(Duration::from_secs(10), SecureChannel::initiate(stream, &key)).await??;
    if ch.remote_static != hex::decode(&dest.pk)? {
        bail!("{} presented an unexpected identity; not sending", dest.name);
    }

    ch.send(&serde_json::to_vec(&header)?).await?;

    let mut buf = vec![0u8; CHUNK];
    let mut left = header.size;
    let mut last = Instant::now();
    while left > 0 {
        let want = std::cmp::min(left, CHUNK as u64) as usize;
        let n = reader.read(&mut buf[..want]).await?;
        if n == 0 {
            bail!("the source ended early");
        }
        ch.send(&buf[..n]).await?;
        left -= n as u64;

        if last.elapsed() >= PROGRESS_EVERY {
            last = Instant::now();
            shared.transfer(
                id,
                TransferDir::Upload,
                label,
                &dest.name,
                total - left,
                total,
                TransferState::Active,
            );
        }
    }

    let ack = ch.recv().await?;
    if ack != b"OK" {
        bail!("{} did not confirm receipt", dest.name);
    }
    Ok(())
}

async fn send_file(
    shared: &Arc<Shared>,
    dest: &Dest,
    path: &Path,
    label: &str,
    id: u64,
) -> Result<()> {
    let meta = tokio::fs::metadata(path).await?;
    if meta.is_dir() {
        bail!("folders are not supported yet");
    }
    if meta.len() > MAX_FILE_BYTES {
        bail!("file is too large");
    }
    let file = tokio::fs::File::open(path).await?;
    let header = ItemHeader {
        kind: ItemKind::File,
        name: label.to_string(),
        size: meta.len(),
    };
    push_item(shared, dest, header, file, label, id).await
}

async fn send_payload(shared: Arc<Shared>, targets: Vec<Dest>, payload: Payload) {
    // Files are announced up front as "queued" so the UI can show the whole batch.
    let file_jobs: Vec<Vec<(u64, String, u64)>> = match &payload {
        Payload::Files(paths) => targets
            .iter()
            .map(|dest| {
                paths
                    .iter()
                    .map(|path| {
                        let id = shared.new_id();
                        let label = path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| "file".to_string());
                        let total = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                        shared.transfer(
                            id,
                            TransferDir::Upload,
                            &label,
                            &dest.name,
                            0,
                            total,
                            TransferState::Queued,
                        );
                        (id, label, total)
                    })
                    .collect()
            })
            .collect(),
        _ => Vec::new(),
    };

    for (di, dest) in targets.iter().enumerate() {
        match &payload {
            Payload::Files(paths) => {
                for (path, (id, label, total)) in paths.iter().zip(&file_jobs[di]) {
                    let res = send_file(&shared, dest, path, label, *id).await;
                    shared.finish_send(*id, label, &dest.name, *total, res);
                }
            }
            Payload::Text(text) => {
                let bytes = text.as_bytes();
                let id = shared.new_id();
                let total = bytes.len() as u64;
                let res = if total > MAX_TEXT_BYTES {
                    Err(anyhow!("text is larger than 1 MB"))
                } else {
                    let header = ItemHeader {
                        kind: ItemKind::Text,
                        name: "Text".to_string(),
                        size: total,
                    };
                    push_item(&shared, dest, header, bytes, "Text", id).await
                };
                shared.finish_send(id, "Text", &dest.name, total, res);
            }
            Payload::Image(png) => {
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let label = format!("Image-{stamp}.png");
                let id = shared.new_id();
                let total = png.len() as u64;
                let header = ItemHeader {
                    kind: ItemKind::Image,
                    name: label.clone(),
                    size: total,
                };
                let res = push_item(&shared, dest, header, &png[..], &label, id).await;
                shared.finish_send(id, &label, &dest.name, total, res);
            }
        }
    }
}

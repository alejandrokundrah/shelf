//! The engine runs on its own thread with a tokio runtime. The UI sends `Command`s
//! and receives `Event`s; `wake` is called after every event so the UI can repaint.

use crate::channel::SecureChannel;
use crate::pairing;
use crate::protocol::{
    ItemHeader, ItemKind, RoomFileRecord, RoomMemberRecord, RoomMessage, CHUNK, MAX_FILE_BYTES,
    MAX_TEXT_BYTES,
};
use crate::store::{inbox_dir, Config, DeviceGroup, PairedPeer, Room, RoomUpdate};
use anyhow::{anyhow, bail, Context, Result};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use rand::Rng;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
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

// ---------------------------------------------------------------- public types

#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    All,
    Peer(String), // public key (hex)
    Peers(Vec<String>), // public keys (hex)
}

#[derive(Clone, Debug)]
pub enum Payload {
    Files(Vec<PathBuf>),
    Text(String),
    Image(Vec<u8>), // PNG bytes
}

pub enum Command {
    CreateRoom { name: String },
    CreateAssignment {
        room_id: String,
        title: String,
        deadline: u64,
        instructions: String,
        required_files: Vec<String>,
    },
    UpdateRoom { room_id: String, update: RoomUpdate },
    JoinRoom {
        addr: SocketAddr,
        room_id: String,
        invite_code: String,
    },
    UploadRoomFiles { room_id: String, paths: Vec<PathBuf> },
    DeleteRoomFile { room_id: String, hash: String },
    RequestRoomFile { room_id: String, name: String },
    SubmitRoomFiles {
        room_id: String,
        paths: Vec<PathBuf>,
        assignment: String,
    },
    StartPairing,
    CancelPairing,
    PairWith { addr: SocketAddr, code: String },
    Unpair { public_key: String },
    Send { target: Target, payload: Payload },
    SetReceivingPaused(bool),
    SetReceiveDirectory(PathBuf),
    SaveDeviceGroup(DeviceGroup),
    ExportRoomBackup { room_id: String, path: PathBuf },
    ImportRoomBackup { path: PathBuf },
    CancelTransfers,
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
    pub room_id: Option<String>,
    pub kind: ItemKind,
    pub name: String,
    pub from: String,
    pub size: u64,
    pub path: Option<PathBuf>,
    pub text: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferDirection {
    Upload,
    Download,
}

pub enum Event {
    RoomUpdated(Room),
    RoomRequest {
        room_id: String,
        name: String,
        from: String,
    },
    Ready { device_name: String, public_key: String },
    Peers { paired: Vec<PeerStatus>, nearby: Vec<NearbyDevice> },
    DeviceGroups(Vec<DeviceGroup>),
    ReceiveDirectory(Option<PathBuf>),
    PairingCode { code: String, seconds: u64 },
    PairingCleared,
    PairingResult(Result<String, String>),
    Received(ReceivedItem),
    Progress {
        direction: TransferDirection,
        name: String,
        peer: String,
        done: u64,
        total: u64,
    },
    SendDone { to: String, item: String, error: Option<String> },
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

#[derive(Clone)]
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
    last_known_addrs: Mutex<HashMap<String, SocketAddr>>,
    next_id: AtomicU64,
    receiving_paused: std::sync::atomic::AtomicBool,
    cancel_transfers: std::sync::atomic::AtomicBool,
    received_hashes: Mutex<HashSet<[u8; 32]>>,
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
        let last_known = self.last_known_addrs.lock().unwrap();
        cfg.peers
            .iter()
            .filter(|p| match t {
                Target::All => true,
                Target::Peer(k) => &p.public_key == k,
                Target::Peers(keys) => keys.iter().any(|k| k == &p.public_key),
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
                    .or_else(|| {
                        last_known.get(&p.public_key).copied().map(|addr| Dest {
                            pk: p.public_key.clone(),
                            name: p.name.clone(),
                            addr,
                        })
                    })
            })
            .collect()
    }

    fn private_key(&self) -> Result<Vec<u8>> {
        let hex_key = self.config.lock().unwrap().private_key.clone();
        Ok(hex::decode(hex_key)?)
    }

    fn create_room(&self, name: String) {
        let result = {
            let mut config = self.config.lock().unwrap();
            let room = config.create_room(name);
            config.save().map(|()| room)
        };
        match result {
            Ok(room) => self.emit(Event::RoomUpdated(room)),
            Err(e) => self.emit(Event::Error(format!("could not create room: {e}"))),
        }
    }

    fn save_device_group(&self, group: DeviceGroup) {
        let result = {
            let mut config = self.config.lock().unwrap();
            config.device_groups.retain(|existing| existing.name != group.name);
            config.device_groups.push(group);
            config.save().map(|()| config.device_groups.clone())
        };
        match result {
            Ok(groups) => self.emit(Event::DeviceGroups(groups)),
            Err(error) => self.emit(Event::Error(format!("could not save device group: {error}"))),
        }
    }

    fn export_room_backup(&self, room_id: String, path: PathBuf) {
        let result = (|| -> Result<()> {
            let room = self
                .config
                .lock()
                .unwrap()
                .rooms
                .iter()
                .find(|room| room.id == room_id)
                .cloned()
                .ok_or_else(|| anyhow!("room not found"))?;
            let output = std::fs::File::create(&path)?;
            let mut archive = zip::ZipWriter::new(output);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            archive.start_file("room.json", options)?;
            archive.write_all(&serde_json::to_vec_pretty(&room)?)?;
            let folder = crate::store::room_dir(&room_id);
            for file in &room.files {
                let source = folder.join(&file.name);
                if source.is_file() {
                    archive.start_file(format!("files/{}", file.name), options)?;
                    let mut input = std::fs::File::open(source)?;
                    std::io::copy(&mut input, &mut archive)?;
                }
            }
            archive.finish()?;
            Ok(())
        })();
        match result {
            Ok(()) => self.emit(Event::Info(format!("Room backup saved to {}", path.display()))),
            Err(error) => self.emit(Event::Error(format!("room backup failed: {error}"))),
        }
    }

    fn import_room_backup(&self, path: PathBuf) {
        let result = (|| -> Result<Room> {
            let input = std::fs::File::open(&path)?;
            let mut archive = zip::ZipArchive::new(input)?;
            let mut room_json = String::new();
            archive.by_name("room.json")?.read_to_string(&mut room_json)?;
            let room: Room = serde_json::from_str(&room_json)?;
            {
                let mut config = self.config.lock().unwrap();
                if config.rooms.iter().any(|existing| existing.id == room.id) {
                    bail!("a room with this ID already exists");
                }
                config.rooms.push(room.clone());
                config.save()?;
            }
            let folder = crate::store::room_dir(&room.id);
            std::fs::create_dir_all(&folder)?;
            for index in 0..archive.len() {
                let mut entry = archive.by_index(index)?;
                let Some(relative) = entry.enclosed_name().map(PathBuf::from) else {
                    bail!("backup contains an unsafe path");
                };
                if let Ok(file_name) = relative.strip_prefix("files") {
                    let destination = folder.join(file_name);
                    if let Some(parent) = destination.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    let mut output = std::fs::File::create(destination)?;
                    std::io::copy(&mut entry, &mut output)?;
                }
            }
            Ok(room)
        })();
        match result {
            Ok(room) => self.emit(Event::RoomUpdated(room)),
            Err(error) => self.emit(Event::Error(format!("room restore failed: {error}"))),
        }
    }

    fn create_assignment(
        &self,
        room_id: String,
        title: String,
        deadline: u64,
        instructions: String,
        required_files: Vec<String>,
    ) {
        let result = {
            let mut config = self.config.lock().unwrap();
            let owner = config.public_key.clone();
            let updated_room = {
                let room = config.rooms.iter_mut().find(|room| room.id == room_id);
                let Some(room) = room else {
                    return self.emit(Event::Error("room not found".into()));
                };
                let Some(_) =
                    room.create_assignment(&owner, title, deadline, instructions, required_files)
                else {
                    return self.emit(Event::Error(
                        "only the room owner can create assignments".into(),
                    ));
                };
                room.clone()
            };
            config.save().map(|()| updated_room)
        };
        match result {
            Ok(room) => self.emit(Event::RoomUpdated(room)),
            Err(error) => self.emit(Event::Error(format!(
                "could not save assignment: {error}"
            ))),
        }
    }

    fn finish_send(&self, to: &str, item: String, res: Result<()>) {
        self.emit(Event::SendDone {
            to: to.to_string(),
            item,
            error: res.err().map(|e| e.to_string()),
        });
    }
}

fn apply_room_update(
    shared: &Arc<Shared>,
    room_id: &str,
    requester: &str,
    update: &RoomUpdate,
) -> Result<(Room, String)> {
    let mut config = shared.config.lock().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    let room = config
        .rooms
        .iter_mut()
        .find(|room| room.id == room_id)
        .ok_or_else(|| anyhow!("room not found"))?;
    if !room.has_member(requester) {
        bail!("room update rejected from a non-member");
    }
    let Some(activity) = room.apply_team_update(requester, update, now) else {
        bail!("room update is not allowed for this member");
    };
    let snapshot = room.clone();
    config.save()?;
    Ok((snapshot, activity))
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
        last_known_addrs: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        receiving_paused: std::sync::atomic::AtomicBool::new(false),
        cancel_transfers: std::sync::atomic::AtomicBool::new(false),
        received_hashes: Mutex::new(HashSet::new()),
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
                    let addr = SocketAddr::new(ip, info.get_port());
                    let dev = NearbyDevice {
                        name: dname,
                        public_key: pk,
                        addr,
                    };
                    let discovered_key = dev.public_key.clone();
                    shared
                        .nearby
                        .lock()
                        .unwrap()
                        .insert(info.get_fullname().to_string(), dev);
                    shared
                        .last_known_addrs
                        .lock()
                        .unwrap()
                        .insert(discovered_key, addr);
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
    {
        let config = shared.config.lock().unwrap();
        shared.emit(Event::DeviceGroups(config.device_groups.clone()));
        shared.emit(Event::ReceiveDirectory(config.receive_dir.clone()));
    }

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
            Command::CreateRoom { name } => shared.create_room(name),
            Command::CreateAssignment {
                room_id,
                title,
                deadline,
                instructions,
                required_files,
            } => shared.create_assignment(
                room_id,
                title,
                deadline,
                instructions,
                required_files,
            ),
            Command::UpdateRoom { room_id, update } => {
                let actor = shared.config.lock().unwrap().public_key.clone();
                match apply_room_update(&shared, &room_id, &actor, &update) {
                    Ok((room, activity)) => {
                        shared.emit(Event::RoomUpdated(room.clone()));
                        shared.emit(Event::Info(format!("Room {activity}")));
                        let targets = room_targets(&shared, &room_id);
                        tokio::spawn(broadcast_room_sync(shared.clone(), targets, room));
                    }
                    Err(error) => shared.emit(Event::Error(error.to_string())),
                }
            }
            Command::JoinRoom {
                addr,
                room_id,
                invite_code,
            } => {
                let s = shared.clone();
                tokio::spawn(async move {
                    if let Err(e) = join_room(&s, addr, &room_id, &invite_code).await {
                        s.emit(Event::Error(format!("room join failed: {e}")));
                    }
                });
            }
            Command::UploadRoomFiles { room_id, paths } => {
                let targets = room_targets(&shared, &room_id);
                if targets.is_empty() {
                    shared.emit(Event::Error("No room members are reachable. Open Shelf on the other devices and try again.".into()));
                } else {
                    let s = shared.clone();
                    tokio::spawn(send_room_files(s, targets, room_id, paths, None));
                }
            }
            Command::DeleteRoomFile { room_id, hash } => {
                let (targets, file_name) = {
                    let config = shared.config.lock().unwrap();
                    match config.rooms.iter().find(|room| room.id == room_id) {
                        Some(room) if room.owner_public_key == config.public_key => {
                            let file_name = room
                                .files
                                .iter()
                                .find(|file| file.hash == hash)
                                .map(|file| file.name.clone());
                            let target = Target::Peers(
                                room.members
                                    .iter()
                                    .filter(|member| member.public_key != config.public_key)
                                    .map(|member| member.public_key.clone())
                                    .collect(),
                            );
                            (shared.targets(&target), file_name)
                        }
                        _ => (Vec::new(), None),
                    }
                };
                if let Some(file_name) = file_name {
                    let path = crate::store::room_dir(&room_id).join(&file_name);
                    let _ = std::fs::remove_file(path);
                    let mut config = shared.config.lock().unwrap();
                    let local_key = config.public_key.clone();
                    if let Some(room) = config.rooms.iter_mut().find(|room| room.id == room_id) {
                        room.delete_file(&local_key, &hash);
                    }
                    config.save().ok();
                    let s = shared.clone();
                    tokio::spawn(broadcast_room_delete(s, targets, room_id, hash));
                    shared.emit(Event::Info(format!("Deleted {file_name} from room")));
                } else {
                    shared.emit(Event::Error("Only the room owner can delete files".into()));
                }
            }
            Command::RequestRoomFile { room_id, name } => {
                let target = {
                    let config = shared.config.lock().unwrap();
                    config
                        .rooms
                        .iter()
                        .find(|room| room.id == room_id)
                        .map(|room| Target::Peer(room.owner_public_key.clone()))
                };
                let targets = target.map(|target| shared.targets(&target)).unwrap_or_default();
                if targets.is_empty() {
                    shared.emit(Event::Error("Room owner is not nearby".into()));
                } else {
                    let s = shared.clone();
                    tokio::spawn(send_room_request(s, targets, room_id, name, None));
                }
            }
            Command::SubmitRoomFiles {
                room_id,
                paths,
                assignment,
            } => {
                let targets = room_targets(&shared, &room_id);
                if targets.is_empty() {
                    shared.emit(Event::Error("No room members are reachable. Open Shelf on the other devices and try again.".into()));
                } else {
                    let s = shared.clone();
                    tokio::spawn(send_room_files(
                        s,
                        targets,
                        room_id,
                        paths,
                        Some(assignment),
                    ));
                }
            }
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
                shared.cancel_transfers.store(false, Ordering::SeqCst);
                let targets = shared.targets(&target);
                if targets.is_empty() {
                    shared.emit(Event::Error("No paired device is nearby right now".into()));
                    continue;
                }
                let s = shared.clone();
                tokio::spawn(send_payload(s, targets, payload));
            }
            Command::SetReceivingPaused(paused) => {
                shared.receiving_paused.store(paused, Ordering::SeqCst);
                shared.emit(Event::Info(if paused {
                    "Receiving paused".into()
                } else {
                    "Receiving resumed".into()
                }));
            }
            Command::SetReceiveDirectory(path) => {
                let result = {
                    let mut config = shared.config.lock().unwrap();
                    config.receive_dir = Some(path.clone());
                    config.save()
                };
                match result {
                    Ok(()) => shared.emit(Event::ReceiveDirectory(Some(path))),
                    Err(error) => shared.emit(Event::Error(format!("could not save receive folder: {error}"))),
                }
            }
            Command::SaveDeviceGroup(group) => shared.save_device_group(group),
            Command::ExportRoomBackup { room_id, path } => shared.export_room_backup(room_id, path),
            Command::ImportRoomBackup { path } => shared.import_room_backup(path),
            Command::CancelTransfers => {
                shared.cancel_transfers.store(true, Ordering::SeqCst);
                shared.emit(Event::Info("Cancelling transfers...".into()));
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
        b'R' => handle_room_connection(&shared, stream).await,
        b'T' => handle_transfer(&shared, stream).await,
        _ => Ok(()),
    };
    if let Err(e) = result {
        shared.emit(Event::Error(e.to_string()));
    }
}

async fn handle_room_connection(shared: &Arc<Shared>, stream: TcpStream) -> Result<()> {
    let key = shared.private_key()?;
    let mut ch = timeout(Duration::from_secs(10), SecureChannel::accept(stream, &key)).await??;
    let peer = {
        let config = shared.config.lock().unwrap();
        config.find_peer(&ch.remote_static).cloned()
    }
    .ok_or_else(|| anyhow!("room connection rejected from an unpaired device"))?;
    let message: RoomMessage = serde_json::from_slice(&ch.recv().await?)?;
    if let RoomMessage::Sync { room } = message {
        let remote_key = hex::encode(&ch.remote_static);
        let accepted = {
            let mut config = shared.config.lock().unwrap();
            let local = config
                .rooms
                .iter_mut()
                .find(|local| local.id == room.id)
                .ok_or_else(|| anyhow!("room not found"))?;
            if local.owner_public_key != room.owner_public_key || !local.has_member(&remote_key) {
                bail!("room snapshot rejected");
            }
            *local = room.clone();
            config.save()?;
            room
        };
        ch.send(b"OK").await?;
        shared.emit(Event::RoomUpdated(accepted));
        return Ok(());
    }
    if let RoomMessage::Update { room_id, update } = message {
        let remote_key = hex::encode(&ch.remote_static);
        let (room, activity) = apply_room_update(shared, &room_id, &remote_key, &update)?;
        ch.send(b"OK").await?;
        let targets = room_targets_except(shared, &room_id, &remote_key);
        tokio::spawn(broadcast_room_sync(shared.clone(), targets, room.clone()));
        shared.emit(Event::RoomUpdated(room));
        shared.emit(Event::Info(format!("{} {}", peer.name, activity)));
        return Ok(());
    }
    if let RoomMessage::Delete { room_id, hash } = message {
        let file_name = {
            let mut config = shared.config.lock().unwrap();
            let local_key = config.public_key.clone();
            let room = config
                .rooms
                .iter_mut()
                .find(|room| room.id == room_id)
                .ok_or_else(|| anyhow!("room not found"))?;
            if room.owner_public_key != hex::encode(&ch.remote_static) {
                bail!("only the room owner may delete files");
            }
            let name = room
                .files
                .iter()
                .find(|file| file.hash == hash)
                .map(|file| file.name.clone());
            room.delete_file(&local_key, &hash);
            config.save()?;
            name
        };
        if let Some(file_name) = file_name {
            let _ = std::fs::remove_file(crate::store::room_dir(&room_id).join(file_name));
        }
        ch.send(b"OK").await?;
        return Ok(());
    }
    if let RoomMessage::Request {
        room_id,
        name,
        hash,
    } = message
    {
        let known_member = {
            let config = shared.config.lock().unwrap();
            config
                .rooms
                .iter()
                .find(|room| room.id == room_id)
                .map(|room| room.has_member(&hex::encode(&ch.remote_static)))
                .unwrap_or(false)
        };
        if !known_member {
            bail!("room request rejected from a non-member");
        }
        ch.send(b"OK").await?;
        if let Some(hash) = hash {
            let transfer = {
                let config = shared.config.lock().unwrap();
                config
                    .rooms
                    .iter()
                    .find(|room| room.id == room_id)
                    .and_then(|room| {
                        room.files
                            .iter()
                            .find(|file| file.hash == hash)
                            .map(|file| {
                                (
                                    crate::store::room_dir(&room_id).join(&file.name),
                                    file.submission.clone(),
                                )
                            })
                    })
            };
            if let Some((path, submission)) = transfer {
                let targets = shared.targets(&Target::Peer(hex::encode(&ch.remote_static)));
                tokio::spawn(send_room_file(
                    shared.clone(),
                    targets,
                    room_id.clone(),
                    path,
                    submission,
                ));
            }
        }
        shared.emit(Event::RoomRequest {
            room_id,
            name,
            from: peer.name,
        });
        return Ok(());
    }
    let RoomMessage::Join {
        room_id,
        invite_code,
        member_name,
        member_public_key,
    } = message
    else {
        bail!("expected room join or delete message");
    };

    let manifest = {
        let mut config = shared.config.lock().unwrap();
        let local_key = config.public_key.clone();
        let room = {
            let room = config
                .rooms
                .iter_mut()
                .find(|room| room.id == room_id)
                .ok_or_else(|| anyhow!("room not found"))?;
            if room.invite_code != invite_code || !room.has_member(&local_key) {
                bail!("invalid room invite");
            }
            room.add_member(crate::store::RoomMember {
                name: member_name,
                public_key: member_public_key,
                role: crate::store::RoomRole::Member,
            });
            room.clone()
        };
        config.save()?;
        RoomMessage::Manifest {
            room_id: room.id.clone(),
            room_name: room.name.clone(),
            invite_code: room.invite_code.clone(),
            owner_public_key: room.owner_public_key.clone(),
            members: room
                .members
                .iter()
                .map(|member| RoomMemberRecord {
                    name: member.name.clone(),
                    public_key: member.public_key.clone(),
                    owner: member.role == crate::store::RoomRole::Owner,
                })
                .collect(),
            files: room
                .files
                .iter()
                .map(|file| RoomFileRecord {
                    hash: file.hash.clone(),
                    name: file.name.clone(),
                    size: file.size,
                    uploaded_by: file.uploaded_by.clone(),
                    uploaded_at: file.uploaded_at,
                    submission: file.submission.clone(),
                })
                .collect(),
            room: Some(room.clone()),
        }
    };
    ch.send(&serde_json::to_vec(&manifest)?).await?;
    shared.emit(Event::Info(format!("{} joined room {}", peer.name, room_id)));
    Ok(())
}

async fn broadcast_room_delete(
    shared: Arc<Shared>,
    targets: Vec<Dest>,
    room_id: String,
    hash: String,
) {
    for dest in targets {
        let result = async {
            let key = shared.private_key()?;
            let mut stream =
                timeout(Duration::from_secs(5), TcpStream::connect(dest.addr)).await??;
            stream.write_u8(b'R').await?;
            let mut ch =
                timeout(Duration::from_secs(10), SecureChannel::initiate(stream, &key)).await??;
            ch.send(
                &serde_json::to_vec(&RoomMessage::Delete {
                    room_id: room_id.clone(),
                    hash: hash.clone(),
                })?,
            )
            .await?;
            if ch.recv().await? != b"OK" {
                bail!("member did not confirm deletion");
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(error) = result {
            shared.emit(Event::Error(format!(
                "room deletion failed for {}: {error}",
                dest.name
            )));
        }
    }
}

async fn broadcast_room_sync(shared: Arc<Shared>, targets: Vec<Dest>, room: Room) {
    for dest in targets {
        let result = async {
            let key = shared.private_key()?;
            let mut stream = timeout(Duration::from_secs(5), TcpStream::connect(dest.addr)).await??;
            stream.write_u8(b'R').await?;
            let mut ch = timeout(Duration::from_secs(10), SecureChannel::initiate(stream, &key)).await??;
            ch.send(&serde_json::to_vec(&RoomMessage::Sync { room: room.clone() })?).await?;
            if ch.recv().await? != b"OK" {
                bail!("member did not confirm room synchronization");
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(error) = result {
            shared.emit(Event::Error(format!("room sync to {} failed: {error}", dest.name)));
        }
    }
}

async fn send_room_request(
    shared: Arc<Shared>,
    targets: Vec<Dest>,
    room_id: String,
    name: String,
    hash: Option<String>,
) {
    for dest in targets {
        let result = async {
            let key = shared.private_key()?;
            let mut stream =
                timeout(Duration::from_secs(5), TcpStream::connect(dest.addr)).await??;
            stream.write_u8(b'R').await?;
            let mut ch =
                timeout(Duration::from_secs(10), SecureChannel::initiate(stream, &key)).await??;
            ch.send(
                &serde_json::to_vec(&RoomMessage::Request {
                    room_id: room_id.clone(),
                    name: name.clone(),
                    hash: hash.clone(),
                })?,
            )
            .await?;
            if ch.recv().await? != b"OK" {
                bail!("room owner rejected request");
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(error) = result {
            shared.emit(Event::Error(format!(
                "file request failed for {}: {error}",
                dest.name
            )));
        }
    }
}

async fn join_room(
    shared: &Arc<Shared>,
    addr: SocketAddr,
    room_id: &str,
    invite_code: &str,
) -> Result<()> {
    let key = shared.private_key()?;
    let config = shared.config.lock().unwrap().clone();
    let mut stream = timeout(Duration::from_secs(5), TcpStream::connect(addr)).await??;
    stream.write_u8(b'R').await?;
    let mut ch = timeout(Duration::from_secs(10), SecureChannel::initiate(stream, &key)).await??;
    if config.find_peer(&ch.remote_static).is_none() {
        bail!("room owner is not paired");
    }
    ch.send(
        &serde_json::to_vec(&RoomMessage::Join {
            room_id: room_id.to_string(),
            invite_code: invite_code.to_string(),
            member_name: config.device_name.clone(),
            member_public_key: config.public_key.clone(),
        })?,
    )
    .await?;
    let response: RoomMessage = serde_json::from_slice(&ch.recv().await?)?;
    let RoomMessage::Manifest {
        room_id,
        room_name,
        invite_code,
        owner_public_key,
        members,
        files,
        room: room_snapshot,
    } = response
    else {
        bail!("room owner returned an invalid response");
    };
    let room = room_snapshot.unwrap_or_else(|| crate::store::Room {
        id: room_id,
        name: room_name,
        invite_code,
        owner_public_key,
        members: members
            .into_iter()
            .map(|member| crate::store::RoomMember {
                name: member.name,
                public_key: member.public_key,
                role: if member.owner {
                    crate::store::RoomRole::Owner
                } else {
                    crate::store::RoomRole::Member
                },
            })
            .collect(),
        files: files
            .into_iter()
            .map(|file| crate::store::RoomFile {
                hash: file.hash,
                name: file.name,
                size: file.size,
                uploaded_by: file.uploaded_by,
                uploaded_at: file.uploaded_at,
                submission: file.submission,
                version: 0,
                previous_hash: None,
                downloads_allowed: true,
                locked_by: None,
            })
            .collect(),
        assignments: Vec::new(),
        questions: Vec::new(),
        session_expires_at: None,
        notes: Vec::new(),
        links: Vec::new(),
        announcements: Vec::new(),
        clipboard: Vec::new(),
        activity: Vec::new(),
        pending_members: Vec::new(),
    });
    let owner_key = room.owner_public_key.clone();
    let room_files = room.files.clone();
    let room_id_for_sync = room.id.clone();
    {
        let mut config = shared.config.lock().unwrap();
        if let Some(existing) = config.rooms.iter_mut().find(|existing| existing.id == room.id) {
            *existing = room.clone();
        } else {
            config.rooms.push(room.clone());
        }
        config.save()?;
    }
    shared.emit(Event::RoomUpdated(room));
    for file in room_files {
        let owner_targets = shared.targets(&Target::Peer(owner_key.clone()));
        send_room_request(
            shared.clone(),
            owner_targets,
            room_id_for_sync.clone(),
            file.name,
            Some(file.hash),
        )
        .await;
    }
    Ok(())
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

fn extract_transfer_archive(
    archive_path: &Path,
    base: &Path,
    original_name: &str,
    folder: bool,
) -> Result<PathBuf> {
    let input = std::fs::File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(input)?;
    if folder {
        let destination = unique_inbox_path(base, original_name);
        std::fs::create_dir_all(&destination)?;
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index)?;
            let relative = entry
                .enclosed_name()
                .ok_or_else(|| anyhow!("archive contains an unsafe path"))?
                .to_path_buf();
            let target = destination.join(relative);
            if entry.is_dir() {
                std::fs::create_dir_all(&target)?;
            } else {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut output = std::fs::File::create(&target)?;
                std::io::copy(&mut entry, &mut output)?;
            }
        }
        Ok(destination)
    } else {
        let destination = unique_inbox_path(base, original_name);
        let mut entry = archive
            .by_index(0)
            .context("compressed transfer did not contain a file")?;
        let mut output = std::fs::File::create(&destination)?;
        std::io::copy(&mut entry, &mut output)?;
        Ok(destination)
    }
}

fn emit_progress(
    shared: &Arc<Shared>,
    direction: TransferDirection,
    name: &str,
    peer: &str,
    done: u64,
    total: u64,
    last_percent: &mut u8,
) {
    let percent = if total == 0 {
        100
    } else {
        ((done * 100) / total) as u8
    };
    if percent != *last_percent && (percent.saturating_sub(*last_percent) >= 5 || percent == 100) {
        *last_percent = percent;
        shared.emit(Event::Progress {
            direction,
            name: name.to_string(),
            peer: peer.to_string(),
            done,
            total,
        });
    }
}

async fn receive_to_file(
    shared: &Arc<Shared>,
    peer: &str,
    name: &str,
    ch: &mut SecureChannel,
    path: &Path,
    size: u64,
    offset: u64,
) -> Result<()> {
    let mut f = if offset == 0 {
        tokio::fs::File::create(path).await?
    } else {
        tokio::fs::OpenOptions::new().append(true).open(path).await?
    };
    let mut left = size.saturating_sub(offset);
    let mut done = offset;
    let mut last_percent = if size == 0 { 0 } else { ((offset * 100) / size) as u8 };
    while left > 0 {
        if shared.cancel_transfers.load(Ordering::SeqCst) {
            bail!("transfer cancelled");
        }
        let chunk = ch.recv().await?;
        if chunk.is_empty() || chunk.len() as u64 > left {
            bail!("corrupt transfer");
        }
        f.write_all(&chunk).await?;
        left -= chunk.len() as u64;
        done += chunk.len() as u64;
        emit_progress(
            shared,
            TransferDirection::Download,
            name,
            peer,
            done,
            size,
            &mut last_percent,
        );
    }
    emit_progress(
        shared,
        TransferDirection::Download,
        name,
        peer,
        size,
        size,
        &mut last_percent,
    );
    f.flush().await?;
    Ok(())
}

async fn handle_transfer(shared: &Arc<Shared>, stream: TcpStream) -> Result<()> {
    if shared.receiving_paused.load(Ordering::SeqCst) {
        bail!("receiving is paused");
    }
    let key = shared.private_key()?;
    let mut ch = timeout(Duration::from_secs(10), SecureChannel::accept(stream, &key)).await??;

    let peer = {
        let c = shared.config.lock().unwrap();
        c.find_peer(&ch.remote_static).cloned()
    };
    let peer = peer.ok_or_else(|| anyhow!("authentication failed: unpaired device"))?;
    shared.emit(Event::Info(format!("Connected to {}", peer.name)));

    let header: ItemHeader = serde_json::from_slice(&ch.recv().await?)?;
    if header.name.len() > 512 {
        bail!("item name too long");
    }
    let limit = if header.kind == ItemKind::Text { MAX_TEXT_BYTES } else { MAX_FILE_BYTES };
    if header.size > limit {
        bail!("item from {} is too large", peer.name);
    }

    let duplicate = shared.received_hashes.lock().unwrap().contains(&header.hash);
    if duplicate {
        ch.send(b"DUPLICATE").await?;
        return Ok(());
    }

    let (path, text) = if header.kind == ItemKind::Text {
        ch.send(b"START").await?;
        let mut buf = Vec::with_capacity(header.size as usize);
        let mut left = header.size;
        let mut done = 0;
        let mut last_percent = 0;
        while left > 0 {
            if shared.cancel_transfers.load(Ordering::SeqCst) {
                bail!("transfer cancelled");
            }
            let chunk = ch.recv().await?;
            if chunk.is_empty() || chunk.len() as u64 > left {
                bail!("corrupt transfer");
            }
            left -= chunk.len() as u64;
            done += chunk.len() as u64;
            buf.extend_from_slice(&chunk);
            emit_progress(
                shared,
                TransferDirection::Download,
                &header.name,
                &peer.name,
                done,
                header.size,
                &mut last_percent,
            );
        }
        emit_progress(
            shared,
            TransferDirection::Download,
            &header.name,
            &peer.name,
            header.size,
            header.size,
            &mut last_percent,
        );
        let text_bytes = if header.compressed {
            let mut decoder = GzDecoder::new(&buf[..]);
            let mut decoded = Vec::new();
            decoder.read_to_end(&mut decoded)?;
            decoded
        } else {
            buf
        };
        let text = String::from_utf8(text_bytes).map_err(|_| anyhow!("text was not valid UTF-8"))?;
        (None, Some(text))
    } else {
        let configured_receive_dir = shared.config.lock().unwrap().receive_dir.clone();
        let base = header
            .room_id
            .as_deref()
            .map(crate::store::room_dir)
            .unwrap_or_else(|| configured_receive_dir.unwrap_or_else(inbox_dir));
        tokio::fs::create_dir_all(&base).await?;
        let partial = base.join(format!(".shelf-{}.part", hex::encode(header.hash)));
        let offset = tokio::fs::metadata(&partial)
            .await
            .map(|m| m.len().min(header.size))
            .unwrap_or(0);
        if offset == header.size && header.size > 0 {
            let _ = tokio::fs::remove_file(&partial).await;
        }
        let offset = if offset == header.size { 0 } else { offset };
        ch.send(format!("RESUME:{offset}").as_bytes()).await?;
        receive_to_file(
            shared,
            &peer.name,
            &header.name,
            &mut ch,
            &partial,
            header.size,
            offset,
        )
        .await?;
        let archive_path = unique_inbox_path(&base, &header.name);
        tokio::fs::rename(&partial, &archive_path).await?;
        let path = if header.compressed {
            let original_name = header
                .original_name
                .as_deref()
                .unwrap_or(&header.name);
            let extracted = extract_transfer_archive(
                &archive_path,
                &base,
                original_name,
                header.folder,
            )?;
            let _ = std::fs::remove_file(&archive_path);
            extracted
        } else {
            archive_path
        };
        (Some(path), None)
    };

    ch.send(b"OK").await?;
    shared.received_hashes.lock().unwrap().insert(header.hash);
    if let Some(room_id) = &header.room_id {
        let mut config = shared.config.lock().unwrap();
        if let Some(room) = config.rooms.iter_mut().find(|room| &room.id == room_id) {
            room.add_file(crate::store::RoomFile {
                hash: hex::encode(header.hash),
                name: header.name.clone(),
                size: header.size,
                uploaded_by: peer.name.clone(),
                uploaded_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or_default(),
                submission: header.submission.clone(),
                version: 0,
                previous_hash: None,
                downloads_allowed: true,
                locked_by: None,
            });
            config.save()?;
        }
    }
    shared.emit(Event::Received(ReceivedItem {
        id: rand::random::<u64>(),
        room_id: header.room_id.clone(),
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
) -> Result<()> {
    let key = shared.private_key()?;
    let mut stream = timeout(Duration::from_secs(5), TcpStream::connect(dest.addr))
        .await
        .map_err(|_| anyhow!("timed out connecting to {} at {}", dest.name, dest.addr))?
        .with_context(|| format!("could not connect to {} at {}", dest.name, dest.addr))?;
    stream.write_u8(b'T').await?;
    let mut ch = timeout(Duration::from_secs(10), SecureChannel::initiate(stream, &key)).await??;
    if ch.remote_static != hex::decode(&dest.pk)? {
        bail!("{} presented an unexpected identity; not sending", dest.name);
    }

    ch.send(&serde_json::to_vec(&header)?).await?;

    let response = ch.recv().await?;
    if response == b"DUPLICATE" {
        return Ok(());
    }
    let offset = if response == b"START" {
        0
    } else if let Some(value) = response.strip_prefix(b"RESUME:") {
        std::str::from_utf8(value)?.parse::<u64>()?.min(header.size)
    } else {
        bail!("receiver rejected transfer");
    };
    let mut skipped = 0;
    let mut skip_buf = vec![0u8; CHUNK];
    while skipped < offset {
        let want = std::cmp::min(offset - skipped, CHUNK as u64) as usize;
        let n = reader.read(&mut skip_buf[..want]).await?;
        if n == 0 {
            bail!("the source ended before the resume offset");
        }
        skipped += n as u64;
    }

    let mut buf = vec![0u8; CHUNK];
    let mut left = header.size - offset;
    let mut last_percent = if header.size == 0 {
        0
    } else {
        ((offset * 100) / header.size) as u8
    };
    while left > 0 {
        if shared.cancel_transfers.load(Ordering::SeqCst) {
            bail!("transfer cancelled");
        }
        if shared.cancel_transfers.load(Ordering::SeqCst) {
            bail!("transfer cancelled");
        }
        let want = std::cmp::min(left, CHUNK as u64) as usize;
        let n = reader.read(&mut buf[..want]).await?;
        if n == 0 {
            bail!("the source ended early");
        }
        ch.send(&buf[..n]).await?;
        left -= n as u64;

        emit_progress(
            shared,
            TransferDirection::Upload,
            label,
            &dest.name,
            header.size - left,
            header.size,
            &mut last_percent,
        );
    }

    let ack = ch.recv().await?;
    if ack == b"DUPLICATE" {
        return Ok(());
    }
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
    room_id: Option<&str>,
    submission: Option<&str>,
) -> Result<()> {
    let prepared = prepare_transfer(path, label)?;
    let source_path = prepared.path.clone();
    let meta = tokio::fs::metadata(&source_path)
        .await
        .with_context(|| format!("source file is unavailable: {}", path.display()))?;
    if meta.len() > MAX_FILE_BYTES {
        cleanup_transfer(&prepared);
        bail!("file is too large");
    }
    let mut hash_reader = tokio::fs::File::open(&source_path)
        .await
        .with_context(|| format!("could not open source file: {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut hash_buf = vec![0u8; CHUNK];
    loop {
        let n = hash_reader.read(&mut hash_buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&hash_buf[..n]);
    }
    let hash: [u8; 32] = hasher.finalize().into();
    let header = ItemHeader {
        kind: ItemKind::File,
        name: label.to_string(),
        size: meta.len(),
        hash,
        room_id: room_id.map(str::to_owned),
        submission: submission.map(str::to_owned),
        compressed: prepared.compressed,
        folder: prepared.folder,
        original_name: Some(prepared.original_name.clone()),
    };
    let mut last_error = None;
    for attempt in 0..3 {
        let file = tokio::fs::File::open(&source_path)
            .await
            .with_context(|| format!("source file disappeared before sending: {}", path.display()))?;
        match push_item(shared, dest, header.clone(), file, label).await {
            Ok(()) => {
                cleanup_transfer(&prepared);
                return Ok(());
            }
            Err(e) => {
                let message = e.to_string();
                if message.contains("cancelled") || message.contains("paused") {
                    return Err(e);
                }
                last_error = Some(e);
                if attempt < 2 {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }
    }
    cleanup_transfer(&prepared);
    Err(last_error.unwrap_or_else(|| anyhow!("transfer failed")))
}

struct PreparedTransfer {
    path: PathBuf,
    original_name: String,
    compressed: bool,
    folder: bool,
    temporary: bool,
}

fn transfer_temp_path(extension: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!("shelf-transfer-{}-{stamp}.{extension}", std::process::id()))
}

fn add_directory_to_zip(
    zip: &mut zip::ZipWriter<std::fs::File>,
    root: &Path,
    current: &Path,
    options: zip::write::SimpleFileOptions,
) -> Result<()> {
    for entry in std::fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path.strip_prefix(root)?.to_string_lossy().replace('\\', "/");
        if path.is_dir() {
            zip.add_directory(format!("{relative}/"), options)?;
            add_directory_to_zip(zip, root, &path, options)?;
        } else if path.is_file() {
            zip.start_file(relative, options)?;
            let mut file = std::fs::File::open(&path)?;
            std::io::copy(&mut file, zip)?;
        }
    }
    Ok(())
}

fn create_zip(source: &Path, entry_name: &str, folder: bool) -> Result<PathBuf> {
    let archive_path = transfer_temp_path("zip");
    let file = std::fs::File::create(&archive_path)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    if folder {
        add_directory_to_zip(&mut zip, source, source, options)?;
    } else {
        zip.start_file(entry_name.replace('\\', "/"), options)?;
        let mut input = std::fs::File::open(source)?;
        std::io::copy(&mut input, &mut zip)?;
    }
    zip.finish()?.flush()?;
    Ok(archive_path)
}

fn prepare_transfer(path: &Path, label: &str) -> Result<PreparedTransfer> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("source file is unavailable: {}", path.display()))?;
    if metadata.is_dir() {
        return Ok(PreparedTransfer {
            path: create_zip(path, "", true)?,
            original_name: label.to_string(),
            compressed: true,
            folder: true,
            temporary: true,
        });
    }
    let archive = create_zip(path, label, false)?;
    let archive_size = std::fs::metadata(&archive)?.len();
    if archive_size < metadata.len() {
        Ok(PreparedTransfer {
            path: archive,
            original_name: label.to_string(),
            compressed: true,
            folder: false,
            temporary: true,
        })
    } else {
        let _ = std::fs::remove_file(archive);
        Ok(PreparedTransfer {
            path: path.to_path_buf(),
            original_name: label.to_string(),
            compressed: false,
            folder: false,
            temporary: false,
        })
    }
}

fn cleanup_transfer(prepared: &PreparedTransfer) {
    if prepared.temporary {
        let _ = std::fs::remove_file(&prepared.path);
    }
}

fn compress_text(bytes: &[u8]) -> Result<(Vec<u8>, bool)> {
    let mut compressed = Vec::new();
    let mut encoder = GzEncoder::new(&mut compressed, Compression::default());
    encoder.write_all(bytes)?;
    encoder.finish()?;
    if compressed.len() < bytes.len() {
        Ok((compressed, true))
    } else {
        Ok((bytes.to_vec(), false))
    }
}

async fn send_payload(shared: Arc<Shared>, targets: Vec<Dest>, payload: Payload) {
    for dest in targets {
        match &payload {
            Payload::Files(paths) => {
                for path in paths {
                    let label = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "file".to_string());
                    let res = send_file(&shared, &dest, path, &label, None, None).await;
                    shared.finish_send(&dest.name, label, res);
                }
            }
            Payload::Text(text) => {
                let bytes = text.as_bytes();
                let res = if bytes.len() as u64 > MAX_TEXT_BYTES {
                    Err(anyhow!("text is larger than 1 MB"))
                } else {
                    match compress_text(bytes) {
                        Ok((payload_bytes, compressed)) => {
                            let header = ItemHeader {
                                kind: ItemKind::Text,
                                name: "Text".to_string(),
                                size: payload_bytes.len() as u64,
                                hash: Sha256::digest(&payload_bytes).into(),
                                room_id: None,
                                submission: None,
                                compressed,
                                folder: false,
                                original_name: None,
                            };
                            push_item(&shared, &dest, header, &payload_bytes[..], "Text").await
                        }
                        Err(error) => Err(error),
                    }
                };
                shared.finish_send(&dest.name, "Text".to_string(), res);
            }
            Payload::Image(png) => {
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let label = format!("Image-{stamp}.png");
                let header = ItemHeader {
                    kind: ItemKind::Image,
                    name: label.clone(),
                    size: png.len() as u64,
                    hash: Sha256::digest(png).into(),
                    room_id: None,
                    submission: None,
                        compressed: false,
                        folder: false,
                        original_name: None,
                };
                let res = push_item(&shared, &dest, header, &png[..], &label).await;
                shared.finish_send(&dest.name, label, res);
            }
        }
    }
}

fn room_targets(shared: &Arc<Shared>, room_id: &str) -> Vec<Dest> {
    let keys = {
        let config = shared.config.lock().unwrap();
        let Some(room) = config.rooms.iter().find(|room| room.id == room_id) else {
            return Vec::new();
        };
        room.members
            .iter()
            .filter(|member| member.public_key != config.public_key)
            .map(|member| member.public_key.clone())
            .collect()
    };
    shared.targets(&Target::Peers(keys))
}

fn room_targets_except(shared: &Arc<Shared>, room_id: &str, excluded: &str) -> Vec<Dest> {
    let keys = {
        let config = shared.config.lock().unwrap();
        let Some(room) = config.rooms.iter().find(|room| room.id == room_id) else {
            return Vec::new();
        };
        room.members
            .iter()
            .filter(|member| member.public_key != config.public_key && member.public_key != excluded)
            .map(|member| member.public_key.clone())
            .collect()
    };
    shared.targets(&Target::Peers(keys))
}

async fn send_room_file(
    shared: Arc<Shared>,
    targets: Vec<Dest>,
    room_id: String,
    path: PathBuf,
    submission: Option<String>,
) {
    let label = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    for dest in targets {
        let result = send_file(
            &shared,
            &dest,
            &path,
            &label,
            Some(&room_id),
            submission.as_deref(),
        )
        .await;
        shared.finish_send(&dest.name, label.clone(), result);
    }
    if let Ok(hash) = hash_file(&path).await {
        let mut config = shared.config.lock().unwrap();
        let uploader = config.device_name.clone();
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        if let Some(room) = config.rooms.iter_mut().find(|room| room.id == room_id) {
            room.add_file(crate::store::RoomFile {
                hash: hex::encode(hash),
                name: label,
                size: std::fs::metadata(&path).map(|m| m.len()).unwrap_or_default(),
                uploaded_by: uploader,
                uploaded_at: timestamp,
                submission,
                version: 0,
                previous_hash: None,
                downloads_allowed: true,
                locked_by: None,
            });
            let _ = config.save();
        }
    }
}

async fn send_room_files(
    shared: Arc<Shared>,
    targets: Vec<Dest>,
    room_id: String,
    paths: Vec<PathBuf>,
    submission: Option<String>,
) {
    for path in paths {
        send_room_file(
            shared.clone(),
            targets.clone(),
            room_id.clone(),
            path,
            submission.clone(),
        )
        .await;
    }
}

async fn hash_file(path: &Path) -> Result<[u8; 32]> {
    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("could not read file for hashing: {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; CHUNK];
    loop {
        let count = file.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize().into())
}

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PairedPeer {
    pub name: String,
    /// Hex of the peer's 32 byte Noise static public key.
    pub public_key: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoomRole {
    Owner,
    Member,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RoomMember {
    pub name: String,
    pub public_key: String,
    pub role: RoomRole,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RoomFile {
    pub hash: String,
    pub name: String,
    pub size: u64,
    pub uploaded_by: String,
    pub uploaded_at: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Room {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub invite_code: String,
    pub owner_public_key: String,
    pub members: Vec<RoomMember>,
    #[serde(default)]
    pub files: Vec<RoomFile>,
}

impl Room {
    pub fn can_delete(&self, public_key: &str) -> bool {
        self.owner_public_key == public_key
    }

    pub fn has_member(&self, public_key: &str) -> bool {
        self.members.iter().any(|m| m.public_key == public_key)
    }

    pub fn add_member(&mut self, member: RoomMember) -> bool {
        if self.has_member(&member.public_key) {
            return false;
        }
        self.members.push(member);
        true
    }

    pub fn add_file(&mut self, file: RoomFile) -> bool {
        if self.files.iter().any(|existing| existing.hash == file.hash) {
            return false;
        }
        self.files.push(file);
        true
    }

    pub fn delete_file(&mut self, requester: &str, hash: &str) -> bool {
        if !self.can_delete(requester) {
            return false;
        }
        let before = self.files.len();
        self.files.retain(|file| file.hash != hash);
        self.files.len() != before
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    pub device_name: String,
    /// Hex. Stored in plain text in config.json (see README, security notes).
    pub private_key: String,
    pub public_key: String,
    #[serde(default)]
    pub peers: Vec<PairedPeer>,
    #[serde(default)]
    pub rooms: Vec<Room>,
}

pub fn data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Shelf")
}

pub fn inbox_dir() -> PathBuf {
    data_dir().join("inbox")
}

pub fn room_dir(room_id: &str) -> PathBuf {
    data_dir().join("rooms").join(room_id)
}

fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

impl Config {
    pub fn create_room(&mut self, name: String) -> Room {
        let id = format!("room-{}", uuid_like_id());
        let room = Room {
            id,
            name,
            invite_code: format!("{:06}", room_code()),
            owner_public_key: self.public_key.clone(),
            members: vec![RoomMember {
                name: self.device_name.clone(),
                public_key: self.public_key.clone(),
                role: RoomRole::Owner,
            }],
            files: Vec::new(),
        };
        self.rooms.push(room.clone());
        room
    }

    pub fn load_or_create() -> Result<Config> {
        let path = config_path();
        if let Ok(text) = std::fs::read_to_string(&path) {
            match serde_json::from_str::<Config>(&text) {
                Ok(cfg) => return Ok(cfg),
                Err(_) => {
                    // Keep a copy of an unreadable file instead of silently destroying pairings.
                    let _ = std::fs::rename(&path, path.with_extension("json.bak"));
                }
            }
        }
        let params: snow::params::NoiseParams = crate::channel::NOISE_PARAMS.parse()?;
        let kp = snow::Builder::new(params).generate_keypair()?;
        let cfg = Config {
            device_name: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Windows PC".to_string()),
            private_key: hex::encode(&kp.private),
            public_key: hex::encode(&kp.public),
            peers: Vec::new(),
            rooms: Vec::new(),
        };
        cfg.save()?;
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(data_dir())?;
        let path = config_path();
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn find_peer(&self, public_key: &[u8]) -> Option<&PairedPeer> {
        let k = hex::encode(public_key);
        self.peers.iter().find(|p| p.public_key == k)
    }
}

fn uuid_like_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{now:x}")
}

fn room_code() -> u32 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    (now % 1_000_000) as u32
}

#[cfg(test)]
mod tests {
    use super::{Room, RoomFile, RoomMember, RoomRole};

    fn room() -> Room {
        Room {
            id: "room-1".into(),
            name: "Science Class".into(),
            invite_code: "123456".into(),
            owner_public_key: "teacher".into(),
            members: vec![RoomMember {
                name: "Student".into(),
                public_key: "student".into(),
                role: RoomRole::Member,
            }],
            files: vec![RoomFile {
                hash: "abc".into(),
                name: "worksheet.pdf".into(),
                size: 42,
                uploaded_by: "student".into(),
                uploaded_at: 1,
            }],
        }
    }

    #[test]
    fn only_owner_can_delete_room_files() {
        let room = room();
        assert!(room.can_delete("teacher"));
        assert!(!room.can_delete("student"));
    }

    #[test]
    fn membership_is_checked_by_public_key() {
        let room = room();
        assert!(room.has_member("student"));
        assert!(!room.has_member("unknown"));
    }

    #[test]
    fn members_can_upload_but_only_owner_can_delete() {
        let mut room = room();
        assert!(room.add_member(RoomMember {
            name: "Second student".into(),
            public_key: "student-2".into(),
            role: RoomRole::Member,
        }));
        assert!(!room.add_member(RoomMember {
            name: "Duplicate".into(),
            public_key: "student".into(),
            role: RoomRole::Member,
        }));
        assert!(room.add_file(RoomFile {
            hash: "def".into(),
            name: "submission.txt".into(),
            size: 8,
            uploaded_by: "student".into(),
            uploaded_at: 2,
        }));
        assert!(!room.delete_file("student", "def"));
        assert!(room.delete_file("teacher", "def"));
    }
}

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

fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

impl Config {
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

#[cfg(test)]
mod tests {
    use super::{Room, RoomFile, RoomMember, RoomRole};

    fn room() -> Room {
        Room {
            id: "room-1".into(),
            name: "Science Class".into(),
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
}

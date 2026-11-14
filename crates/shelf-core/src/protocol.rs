use serde::{Deserialize, Serialize};
use crate::store::{Room, RoomUpdate};

/// Plaintext bytes per encrypted frame (Noise limit is 65535 including the 16 byte tag).
pub const CHUNK: usize = 60_000;
pub const MAX_TEXT_BYTES: u64 = 1024 * 1024;
pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024 * 1024;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum ItemKind {
    File,
    Text,
    Image,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ItemHeader {
    pub kind: ItemKind,
    pub name: String,
    pub size: u64,
    pub hash: [u8; 32],
    #[serde(default)]
    pub room_id: Option<String>,
    #[serde(default)]
    pub submission: Option<String>,
    #[serde(default)]
    pub compressed: bool,
    #[serde(default)]
    pub folder: bool,
    #[serde(default)]
    pub original_name: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RoomFileRecord {
    pub hash: String,
    pub name: String,
    pub size: u64,
    pub uploaded_by: String,
    pub uploaded_at: u64,
    #[serde(default)]
    pub submission: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RoomMemberRecord {
    pub name: String,
    pub public_key: String,
    pub owner: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum RoomMessage {
    Join {
        room_id: String,
        invite_code: String,
        member_name: String,
        member_public_key: String,
    },
    Manifest {
        room_id: String,
        #[serde(default)]
        room_name: String,
        #[serde(default)]
        invite_code: String,
        #[serde(default)]
        owner_public_key: String,
        #[serde(default)]
        members: Vec<RoomMemberRecord>,
        files: Vec<RoomFileRecord>,
        #[serde(default)]
        room: Option<Room>,
    },
    Upload {
        room_id: String,
        file: RoomFileRecord,
    },
    Delete {
        room_id: String,
        hash: String,
    },
    Request {
        room_id: String,
        name: String,
        #[serde(default)]
        hash: Option<String>,
    },
    Submit {
        room_id: String,
        assignment: String,
        file: RoomFileRecord,
    },
    Update {
        room_id: String,
        update: RoomUpdate,
    },
    Sync {
        room: Room,
    },
}

#[cfg(test)]
mod tests {
    use super::{RoomFileRecord, RoomMessage};

    #[test]
    fn room_messages_round_trip() {
        let message = RoomMessage::Manifest {
            room_id: "room-1".into(),
            room_name: "Science Class".into(),
            invite_code: "123456".into(),
            owner_public_key: "teacher".into(),
            members: Vec::new(),
            files: vec![RoomFileRecord {
                hash: "abc".into(),
                name: "notes.txt".into(),
                size: 12,
                uploaded_by: "student".into(),
                uploaded_at: 42,
                submission: None,
            }],
            room: None,
        };
        let encoded = serde_json::to_vec(&message).unwrap();
        let decoded: RoomMessage = serde_json::from_slice(&encoded).unwrap();
        assert!(matches!(decoded, RoomMessage::Manifest { .. }));
    }
}

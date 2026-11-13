use serde::{Deserialize, Serialize};

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
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RoomFileRecord {
    pub hash: String,
    pub name: String,
    pub size: u64,
    pub uploaded_by: String,
    pub uploaded_at: u64,
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
        files: Vec<RoomFileRecord>,
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
    },
    Submit {
        room_id: String,
        assignment: String,
        file: RoomFileRecord,
    },
}

#[cfg(test)]
mod tests {
    use super::{RoomFileRecord, RoomMessage};

    #[test]
    fn room_messages_round_trip() {
        let message = RoomMessage::Manifest {
            room_id: "room-1".into(),
            files: vec![RoomFileRecord {
                hash: "abc".into(),
                name: "notes.txt".into(),
                size: 12,
                uploaded_by: "student".into(),
                uploaded_at: 42,
            }],
        };
        let encoded = serde_json::to_vec(&message).unwrap();
        let decoded: RoomMessage = serde_json::from_slice(&encoded).unwrap();
        assert!(matches!(decoded, RoomMessage::Manifest { .. }));
    }
}

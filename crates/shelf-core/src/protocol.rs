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

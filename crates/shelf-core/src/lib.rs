//! shelf-core: discovery, pairing, encryption and transfer for Shelf.
//! The UI talks to this crate only through `start()`, `Command` and `Event`.

pub mod channel;
mod dpapi;
pub mod engine;
pub mod pairing;
pub mod protocol;
pub mod store;

pub use engine::{
    start, Command, Event, Handle, NearbyDevice, Payload, PeerStatus, ReceivedItem, Target,
    TransferDirection,
};
pub use protocol::{ItemKind, RoomMessage};
pub use store::{data_dir, inbox_dir, room_dir};
pub use store::{Room, RoomFile, RoomMember, RoomRole, RoomUpdate};

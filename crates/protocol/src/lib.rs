//! Wire protocol between the mmspad server and app.
//!
//! This crate defines the packet types used for communication between
//! the Linux server and the Android app.

mod packets;
mod codec;

pub use packets::*;
pub use codec::*;

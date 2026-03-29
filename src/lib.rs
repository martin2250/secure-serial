#![no_std]
#![doc = include_str!("../README.md")]
#![allow(async_fn_in_trait, dead_code)]

mod protocol;
mod receiver;
mod resources;
mod sender;
mod transport;

pub use protocol::{Ack, CHUNK_LEN_MAX};
pub use receiver::{run_read, run_write};
pub use resources::{SecureSerialResources, SecureSerialResourcesDefault};
pub use sender::SecureSerialSender;
pub use transport::{CrcDevice, TransportRead, TransportWrite};

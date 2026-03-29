#![no_std]
#![allow(async_fn_in_trait, dead_code)]

mod protocol;
mod receiver;
mod sender;
mod transport;

pub use protocol::{Ack, CHUNK_LEN_MAX};
pub use receiver::{run_read, run_write};
pub use sender::SecureSerialSender;
pub use transport::{CrcDevice, TransportRead, TransportWrite};

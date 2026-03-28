#![no_std]
#![allow(async_fn_in_trait, dead_code)]

mod protocol;
mod receiver;
mod sender;
mod transport;

pub use protocol::Ack;
pub use receiver::SecureSerialReceiver;
pub use sender::SecureSerialSender;
pub use transport::{CrcDevice, TransportRead, TransportWrite};

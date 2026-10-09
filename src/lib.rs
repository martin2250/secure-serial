#![no_std]
#![doc = include_str!("../README.md")]
#![allow(async_fn_in_trait, dead_code)]

mod transport;

pub use transport::{IterRx, TransportRx};

pub trait CrcDevice {
    fn crc(&mut self, data: &[u8]) -> u32;
}

#[cfg(feature = "software-crc")]
struct SoftwareCrc;

#[cfg(feature = "software-crc")]
impl CrcDevice for SoftwareCrc {
    fn crc(&mut self, data: &[u8]) -> u32 {
        // use crc crate to calculate Ethernet / zlib CRC
        todo![];
    }
}

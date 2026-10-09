#![no_std]
#![doc = include_str!("../README.md")]
#![allow(async_fn_in_trait, dead_code)]

mod transport;

pub use transport::{IterRx, TransportRx, TransportTx, TxError};

/// Computes the 32-bit checksum used by the sender and receiver.
///
/// Both peers must use the same CRC algorithm and parameters. Every call is an
/// independent calculation over the entire supplied slice, including the frame's
/// preamble, length, and payload, but excluding its trailer. Hardware devices
/// must reset their CRC state for each call. Recovery can invoke this method for
/// several overlapping candidates; results must not depend on previous calls.
pub trait CrcDevice {
    /// Returns the checksum as a numeric value; the wire trailer stores it little-endian.
    fn crc(&mut self, data: &[u8]) -> u32;
}

#[cfg(feature = "software-crc")]
struct SoftwareCrc;

#[cfg(feature = "software-crc")]
impl CrcDevice for SoftwareCrc {
    fn crc(&mut self, data: &[u8]) -> u32 {
        const CRC: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);
        CRC.checksum(data)
    }
}

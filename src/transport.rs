//! Traits for byte I/O and CRC computation used by [`crate::run_read`] and [`crate::run_write`].
//!
//! Implement these for your UART driver, USB bulk endpoint, or a host-side test double.

/// Async byte input: fills `data` and returns how many bytes were read.
pub trait TransportRead {
    type Error;
    async fn read(&mut self, data: &mut [u8]) -> Result<usize, Self::Error>;
}

/// Async byte output: writes the full slice (implementations may fragment internally).
pub trait TransportWrite {
    type Error;
    async fn write(&mut self, data: &[u8]) -> Result<(), Self::Error>;
}

/// CRC over the bytes that precede the 4-byte little-endian CRC trailer on the wire.
pub trait CrcDevice {
    async fn crc(&mut self, data: &[u8]) -> u32;
}

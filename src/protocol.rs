//! Wire format constants and the in-band acknowledgement record.
//!
//! Frames begin with [`MAGIC`], followed by a length byte, packet type, and payload; see the crate
//! README for the full layout. [`CHUNK_LEN_MAX`] is the maximum size of one encoded chunk buffer
//! (including magic, headers, payload, and CRC trailer accounting).

/// Maximum application payload bytes per wire chunk (excluding framing).
pub const CHUNK_PAYLOAD_MAX: usize = 128;

/// Used as a packet start identifier (SEcure SErial).
pub const MAGIC_0: u8 = 0x5E;
pub const MAGIC_1: u8 = 0x5E;
pub const MAGIC: [u8; 2] = [MAGIC_0, MAGIC_1];
pub const PACKET_DATA: u8 = 0xDA;
pub const PACKET_ACK: u8 = 0xAC;
/// Upper bound on the size of one fully encoded wire chunk (headers + payload + CRC), in bytes.
pub const CHUNK_LEN_MAX: usize = 2 // MAGIC
    + 1 // len
    + 1 // type
    + 2 // packet_id
    + 4 // packet len
    + 4 // chunk offset
    + CHUNK_PAYLOAD_MAX // data
    + 4; // crc

/// Identifies which chunk of a packet the receiver has accepted (used in `ACK` wire frames).
#[derive(Debug)]
pub struct Ack {
    pub(crate) packet_id: u16,
    pub(crate) chunk_offset: u32,
}

impl Ack {
    /// Serializes this ACK to six little-endian bytes (`packet_id`, `chunk_offset`).
    pub fn to_buffer(&self) -> [u8; 6] {
        let mut buffer = [0; 6];
        buffer[0..2].copy_from_slice(&self.packet_id.to_le_bytes());
        buffer[2..6].copy_from_slice(&self.chunk_offset.to_le_bytes());
        buffer
    }

    /// Decodes [`Ack::to_buffer`] output.
    pub fn from_buffer(buffer: [u8; 6]) -> Self {
        let packet_id = u16::from_le_bytes(buffer[0..2].try_into().unwrap());
        let chunk_offset = u32::from_le_bytes(buffer[2..6].try_into().unwrap());

        Self {
            packet_id,
            chunk_offset,
        }
    }
}

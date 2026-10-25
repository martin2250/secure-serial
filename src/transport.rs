//! Build CRC-framed packets in place and receive them from arbitrary stream chunks.
//!
//! A frame contains `55 99`, a two-byte little-endian payload length, the
//! payload, and a four-byte little-endian CRC. The CRC covers the preamble,
//! length, and payload. Payloads may be empty; frames occupy at least eight bytes.
//!
//! Both incoming slices and buffered bytes are inspected by [`scan`]. Complete
//! incoming frames are returned without copying. Incomplete candidates are
//! retained across calls to [`TransportRx::process`]. Continuation first completes
//! and validates the header, then copies only the bytes needed by its length.
//! When the combined input is shorter than eight bytes, all available bytes are
//! copied and rescanned before the parser waits for another chunk.
//!
//! After [`IterRx::next`] returns `None`, the receiver is empty or contains one
//! incomplete candidate at buffer offset zero. Its available preamble bytes are
//! valid; a complete header declares a total size within the buffer capacity.
//! The candidate's CRC has not yet been verified. A plausible false length can
//! delay recovery until enough bytes arrive to check the candidate's CRC.
//!
//! During recovery, buffered bytes may contain multiple frames embedded in a
//! rejected candidate. Returned payloads borrow the parser, so compaction is
//! deferred until the next call. The stable incomplete-candidate invariant
//! therefore applies after exhaustion, rather than after every yielded payload.

use crate::CrcDevice;

/// The two bytes marking a possible frame boundary.
const PREAMBLE: [u8; 2] = [0x55, 0x99];
/// Preamble plus the little-endian `u16` payload length.
const PACKET_HEADER_LEN: usize = PREAMBLE.len() + 2; // preamble (2) + len (2)
/// Size of the little-endian CRC trailer.
const PACKET_CRC_LEN: usize = 4;
/// Frame overhead, including the header and CRC but excluding the payload.
const PACKET_LEN_MIN: usize = PACKET_HEADER_LEN + PACKET_CRC_LEN;

/// An error finalizing a transmit packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum TxError {
    /// The supplied payload length exceeds the buffer or wire-format limit.
    PayloadTooLarge,
}

impl core::fmt::Display for TxError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::PayloadTooLarge => f.write_str("payload length exceeds transmit capacity"),
        }
    }
}

impl core::error::Error for TxError {}

/// A reusable fixed-capacity packet builder with no heap allocation.
///
/// `BUFFER` includes the four-byte header and four-byte CRC. Write directly into
/// [`Self::payload_mut`], then pass the number of written bytes to [`Self::finish`].
/// Finalization adds framing in place without copying the payload.
///
/// Send each returned packet completely before reusing this builder. Consecutive
/// packets need no separator and can be parsed by [`TransportRx`]. One builder
/// holds one packet at a time; its returned slice borrows the reusable storage.
pub struct TransportTx<const BUFFER: usize = 512> {
    buffer: [u8; BUFFER],
}

impl<const BUFFER: usize> Default for TransportTx<BUFFER> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const BUFFER: usize> TransportTx<BUFFER> {
    /// Creates storage for frames of up to `BUFFER` bytes, initialized once.
    ///
    /// A capacity of eight supports only empty payloads. The default capacity
    /// of 512 supports payloads of up to 504 bytes.
    ///
    /// # Panics
    ///
    /// Panics if `BUFFER` is less than eight bytes.
    pub const fn new() -> Self {
        assert!(
            BUFFER >= PACKET_LEN_MIN,
            "transport buffer must hold at least the packet header and CRC (8 bytes)"
        );
        Self {
            buffer: [0; BUFFER],
        }
    }

    /// Maximum payload length, limited by storage and the wire's `u16` length.
    pub const fn payload_capacity(&self) -> usize {
        let capacity = BUFFER - PACKET_LEN_MIN;
        if capacity < u16::MAX as usize {
            capacity
        } else {
            u16::MAX as usize
        }
    }

    /// Returns the writable payload region, reserving space for header and CRC.
    ///
    /// Storage is not cleared between packets and may contain earlier payload or
    /// CRC bytes. Write every byte included in the length passed to [`Self::finish`].
    pub const fn payload_mut(&mut self) -> &mut [u8] {
        let capacity = self.payload_capacity();
        self.buffer
            .split_at_mut(PACKET_HEADER_LEN)
            .1
            .split_at_mut(capacity)
            .0
    }

    /// Writes the header and CRC in place and returns the complete packet.
    ///
    /// `payload_len` counts only payload bytes, all of which the caller must have
    /// written. The CRC covers the header and that payload; its little-endian
    /// trailer follows immediately. Empty payloads are valid.
    ///
    /// An oversized length returns [`TxError::PayloadTooLarge`] without changing
    /// storage or calling the CRC device. The returned slice borrows only this
    /// transmitter, leaving the CRC device available for further use.
    ///
    #[cfg_attr(
        feature = "software-crc",
        doc = r#"
```compile_fail
use secure_serial::{SoftwareCrc, TransportTx};
let mut tx = TransportTx::<32>::new();
let packet = tx.finish(0, &mut SoftwareCrc).unwrap();
tx.payload_mut()[0] = 1;
assert_eq!(packet.len(), 8); // The packet still borrows the transmitter.
```
"#
    )]
    pub fn finish<'a, CRC: CrcDevice>(
        &'a mut self,
        payload_len: usize,
        crc: &mut CRC,
    ) -> Result<&'a [u8], TxError> {
        let max = self.payload_capacity();
        if payload_len > max {
            #[cfg(feature = "defmt")]
            defmt::debug!(
                "TX rejected: payload length {} exceeds capacity {}",
                payload_len,
                max
            );
            return Err(TxError::PayloadTooLarge);
        }
        self.buffer[..PREAMBLE.len()].copy_from_slice(&PREAMBLE);
        self.buffer[PREAMBLE.len()..PACKET_HEADER_LEN]
            .copy_from_slice(&(payload_len as u16).to_le_bytes());
        let crc_start = PACKET_HEADER_LEN + payload_len;
        let checksum = crc.crc(&self.buffer[..crc_start]);
        let packet_end = crc_start + PACKET_CRC_LEN;
        self.buffer[crc_start..packet_end].copy_from_slice(&checksum.to_le_bytes());
        #[cfg(feature = "defmt")]
        defmt::debug!("TX packet prepared: {} payload bytes", payload_len);
        Ok(&self.buffer[..packet_end])
    }
}

/// A fixed-capacity receiver retaining incomplete frames between chunks.
///
/// `BUFFER` limits the entire frame, including its eight-byte overhead. It must
/// be at least eight; the maximum payload size is `min(BUFFER - 8, 65535)`.
/// Frames exceeding this limit are rejected even if they arrive in one chunk.
/// Chunk sizes themselves are unrestricted and may contain several frames.
///
/// This type allocates no heap storage. Use [`Self::process`] for each chunk,
/// exhausting the returned [`IterRx`] before supplying the next chunk.
pub struct TransportRx<const BUFFER: usize = 512> {
    /// Storage for an incomplete candidate or bytes awaiting CRC recovery.
    buffer: [u8; BUFFER],
    /// Number of unconsumed bytes in `buffer[start..start + count]`.
    /// At exhaustion these bytes describe at most one incomplete candidate.
    count: usize,
    /// Beginning of the unconsumed region. Advanced when yielding a buffered
    /// payload so its storage remains intact until the caller releases its borrow.
    start: usize,
}

impl<const BUFFER: usize> Default for TransportRx<BUFFER> {
    /// Creates an empty receiver with the same capacity requirement as [`Self::new`].
    fn default() -> Self {
        Self::new()
    }
}

impl<const BUFFER: usize> TransportRx<BUFFER> {
    /// Creates an empty receiver with storage for frames of up to `BUFFER` bytes.
    ///
    /// A capacity of eight accepts only empty payloads. The default capacity
    /// of 512 accepts payloads of up to 504 bytes.
    ///
    /// # Panics
    ///
    /// Panics if `BUFFER` is less than 8 bytes.
    pub const fn new() -> Self {
        assert!(
            BUFFER >= PACKET_LEN_MIN,
            "transport buffer must hold at least the packet header and CRC (8 bytes)"
        );
        Self {
            buffer: [0; _],
            count: 0,
            start: 0,
        }
    }

    /// Creates a lending parser for the next chunk of stream bytes.
    ///
    /// Call [`IterRx::next`] until it returns `None`. Parsing is lazy: this method
    /// alone neither consumes input nor copies bytes. Exhaustion preserves any
    /// incomplete candidate for a later chunk. An empty chunk can still yield
    /// complete packets left in the buffer during recovery.
    ///
    /// The parser borrows the receiver, chunk, and CRC device. Handle each payload
    /// before requesting the next, or copy it into caller-owned storage to retain
    /// it. Drop the parser before calling this method again.
    ///
    /// Dropping the parser early loses its remaining uncopied input. Bytes already
    /// retained by the receiver survive, including buffered recovery candidates.
    pub const fn process<'a, CRC: CrcDevice>(
        &'a mut self,
        new_data: &'a [u8],
        crc: &'a mut CRC,
    ) -> IterRx<'a, CRC, BUFFER> {
        IterRx {
            new_data,
            transport: self,
            crc,
        }
    }
}

/// A lending parser over one incoming chunk and a receiver's retained bytes.
///
/// Despite its name, this type does not implement [`Iterator`]: payloads may
/// borrow reusable receiver storage. Iterate with
/// `while let Some(payload) = parser.next() { /* handle payload */ }`.
/// [`Self::next`] returns no header or CRC bytes, only validated payloads.
pub struct IterRx<'a, CRC: CrcDevice, const BUFFER: usize = 512> {
    /// Remaining input, advanced whenever bytes are copied or a frame is yielded.
    new_data: &'a [u8],
    /// Receiver storage shared by successive chunks.
    transport: &'a mut TransportRx<BUFFER>,
    /// Device used for independent CRC checks of complete candidates.
    crc: &'a mut CRC,
}

impl<CRC: CrcDevice, const BUFFER: usize> IterRx<'_, CRC, BUFFER> {
    /// Returns the next valid payload, borrowing this parser until the slice is released.
    ///
    /// `Some(&[])` is a valid empty payload. `None` means this chunk is exhausted:
    /// any remaining candidate needs additional bytes and has been retained.
    /// Further calls on the same exhausted parser also return `None`.
    ///
    /// Invalid preambles, lengths exceeding capacity, and failed CRCs trigger a
    /// new search one byte after the rejected start. This preserves overlapping
    /// preambles and packets nested in corrupt frames. A candidate with a valid
    /// header is allowed to wait for its entire declared size before CRC checking.
    ///
    /// This is a lending method rather than `Iterator::next`: a later call may
    /// reuse the storage holding a payload assembled from multiple chunks.
    ///
    #[cfg_attr(
        feature = "software-crc",
        doc = r#"
```compile_fail
use secure_serial::{SoftwareCrc, TransportRx};
let mut rx = TransportRx::<32>::new();
let mut crc = SoftwareCrc;
let mut parser = rx.process(&[0x55, 0x99, 0, 0, 0xF3, 0xA3, 0x55, 0x2F], &mut crc);
let first = parser.next().unwrap();
let second = parser.next();
assert!(first.is_empty()); // The earlier payload still borrows the parser.
```
"#
    )]
    #[allow(clippy::should_implement_trait)] // Iterator cannot express the payload's borrow.
    pub fn next(&mut self) -> Option<&[u8]> {
        let transport = &mut *self.transport;
        loop {
            // With no retained candidate, scan input directly and copy only an
            // incomplete suffix. Complete packets keep their original storage.
            if transport.count == 0 {
                transport.start = 0;
                let Some(candidate) = scan::<CRC, BUFFER>(self.new_data, self.crc) else {
                    self.new_data = &[];
                    return None;
                };
                self.new_data = &self.new_data[candidate.start..];
                match candidate.state {
                    State::Incomplete { .. } => {
                        // scan guarantees a partial header (< 4 bytes) or a
                        // partial frame shorter than its accepted size <= BUFFER.
                        let count = self.new_data.len();
                        transport.buffer[..count].copy_from_slice(self.new_data);
                        transport.count = count;
                        self.new_data = &[];
                        return None;
                    }
                    State::Complete { payload_len } => {
                        let payload = &self.new_data[PACKET_HEADER_LEN..][..payload_len];
                        self.new_data = &self.new_data[PACKET_LEN_MIN + payload_len..];
                        #[cfg(feature = "defmt")]
                        defmt::debug!("RX packet received: {} payload bytes", payload_len);
                        return Some(payload);
                    }
                }
            }

            // A previously returned payload may have left bytes awaiting recovery.
            // Compact only after its borrow has ended.
            if transport.start != 0 {
                transport
                    .buffer
                    .copy_within(transport.start..transport.start + transport.count, 0);
                transport.start = 0;
            }
            let Some(candidate) =
                scan::<CRC, BUFFER>(&transport.buffer[..transport.count], self.crc)
            else {
                transport.count = 0;
                continue;
            };
            match candidate.state {
                State::Complete { payload_len } => {
                    // Retain the suffix for another scan, but leave the returned
                    // payload untouched until its lending borrow ends.
                    let consumed = candidate.start + PACKET_LEN_MIN + payload_len;
                    transport.start = consumed;
                    transport.count -= consumed;
                    let payload_start = candidate.start + PACKET_HEADER_LEN;
                    #[cfg(feature = "defmt")]
                    defmt::debug!("RX buffered packet received: {} payload bytes", payload_len);
                    return Some(&transport.buffer[payload_start..payload_start + payload_len]);
                }
                State::Incomplete { needed } => {
                    // Keep just the surviving candidate, including overlapping preambles.
                    transport
                        .buffer
                        .copy_within(candidate.start..transport.count, 0);
                    transport.count -= candidate.start;
                    if self.new_data.is_empty() {
                        return None;
                    }
                    let copied =
                        if self.new_data.len() < PACKET_LEN_MIN.saturating_sub(transport.count) {
                            // Fewer than eight combined bytes cannot form a complete packet.
                            // Scan again after copying so an invalid header is never retained.
                            self.new_data.len()
                        } else {
                            // First complete the header, then copy only the declared packet.
                            (needed - transport.count).min(self.new_data.len())
                        };
                    let end = transport.count + copied;
                    transport.buffer[transport.count..end]
                        .copy_from_slice(&self.new_data[..copied]);
                    transport.count = end;
                    self.new_data = &self.new_data[copied..];
                }
            }
        }
    }
}

/// The earliest surviving candidate, after all preceding starts were rejected.
struct Candidate {
    /// Offset in the scanned slice; all preceding bytes may be discarded.
    start: usize,
    /// Whether more bytes are required or the whole frame passed its CRC check.
    state: State,
}

/// Result of inspecting a candidate with every currently available byte.
enum State {
    /// Valid available preamble bytes and, if complete, a length within capacity.
    Incomplete {
        /// Required candidate size for the next check: four for an incomplete
        /// header, otherwise the declared whole-frame size. Always exceeds the
        /// candidate's current byte count.
        needed: usize,
    },
    /// Complete header and payload followed by a matching CRC trailer.
    Complete {
        /// Number of payload bytes, excluding the four-byte header and CRC.
        payload_len: usize,
    },
}

/// Finds the earliest incomplete candidate or CRC-valid frame without copying.
///
/// Shared by the direct-input and buffered paths. Rejected starts advance by one
/// byte, preserving overlaps. `None` means no possible preamble remains and all
/// bytes may be discarded. An incomplete result includes the entire suffix from
/// its start, whose length is strictly smaller than `needed` and fits `BUFFER`.
/// A complete result can have arbitrary trailing bytes, which must be rescanned.
///
/// CRC is checked only when the declared frame is fully available. Each check
/// covers the header and payload and compares against a little-endian trailer.
/// The caller guarantees `BUFFER >= PACKET_LEN_MIN` through receiver construction.
fn scan<CRC: CrcDevice, const BUFFER: usize>(data: &[u8], crc: &mut CRC) -> Option<Candidate> {
    let mut start = 0;
    while let Some(offset) = data[start..].iter().position(|&byte| byte == PREAMBLE[0]) {
        #[cfg(feature = "defmt")]
        if offset != 0 {
            defmt::debug!("RX discarded {} bytes before preamble", offset);
        }
        start += offset;
        let candidate = &data[start..];
        if candidate.get(1).is_some_and(|&byte| byte != PREAMBLE[1]) {
            #[cfg(feature = "defmt")]
            defmt::debug!(
                "RX rejected: invalid preamble second byte {=u8:#04x}; resuming scan",
                candidate[1]
            );
            start += 1;
            continue;
        }
        if candidate.len() < PACKET_HEADER_LEN {
            return Some(Candidate {
                start,
                state: State::Incomplete {
                    needed: PACKET_HEADER_LEN,
                },
            });
        }
        let payload_len = u16::from_le_bytes([candidate[2], candidate[3]]) as usize;
        // Reject before adding overhead, which could overflow a 16-bit usize.
        if payload_len > BUFFER - PACKET_LEN_MIN {
            #[cfg(feature = "defmt")]
            defmt::debug!(
                "RX rejected: payload length {} exceeds capacity {}; resuming scan",
                payload_len,
                BUFFER - PACKET_LEN_MIN
            );
            start += 1;
            continue;
        }
        let total = payload_len + PACKET_LEN_MIN;
        if candidate.len() < total {
            return Some(Candidate {
                start,
                state: State::Incomplete { needed: total },
            });
        }
        let crc_start = PACKET_HEADER_LEN + payload_len;
        let trailer = &candidate[crc_start..total];
        let received = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
        let expected = crc.crc(&candidate[..crc_start]);
        if received == expected {
            return Some(Candidate {
                start,
                state: State::Complete { payload_len },
            });
        }
        #[cfg(feature = "defmt")]
        defmt::debug!(
            "RX rejected: CRC mismatch for {} payload bytes (received {=u32:#010x}, expected {=u32:#010x}); resuming scan",
            payload_len,
            received,
            expected
        );
        start += 1;
    }
    #[cfg(feature = "defmt")]
    if start < data.len() {
        defmt::debug!("RX discarded {} bytes without preamble", data.len() - start);
    }
    None
}

#[cfg(test)]
#[path = "../tests/unit/transport.rs"]
mod tests;

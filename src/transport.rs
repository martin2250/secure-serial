//! Receive CRC-framed packets from a byte stream split into arbitrary chunks.
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
    pub fn new() -> Self {
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
    pub fn process<'a, CRC: CrcDevice>(
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
    /// ```compile_fail
    /// use secure_serial::{CrcDevice, TransportRx};
    /// struct Crc;
    /// impl CrcDevice for Crc {
    ///     fn crc(&mut self, _: &[u8]) -> u32 { 0 }
    /// }
    /// let mut rx = TransportRx::<32>::new();
    /// let mut crc = Crc;
    /// let mut parser = rx.process(&[0x55, 0x99, 0, 0, 0, 0, 0, 0], &mut crc);
    /// let first = parser.next().unwrap();
    /// let second = parser.next();
    /// assert!(first.is_empty()); // The earlier payload still borrows the parser.
    /// ```
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
        start += offset;
        let candidate = &data[start..];
        if candidate.get(1).is_some_and(|&byte| byte != PREAMBLE[1]) {
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
        let total = payload_len + PACKET_LEN_MIN;
        if total > BUFFER {
            start += 1;
            continue;
        }
        if candidate.len() < total {
            return Some(Candidate {
                start,
                state: State::Incomplete { needed: total },
            });
        }
        let crc_start = PACKET_HEADER_LEN + payload_len;
        let trailer = &candidate[crc_start..total];
        let received = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
        if received == crc.crc(&candidate[..crc_start]) {
            return Some(Candidate {
                start,
                state: State::Complete { payload_len },
            });
        }
        start += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::{vec, vec::Vec};

    /// Deterministic test checksum; validates framing rather than a CRC algorithm.
    struct TestCrc;

    impl CrcDevice for TestCrc {
        fn crc(&mut self, data: &[u8]) -> u32 {
            data.iter().fold(0x12345678u32, |crc, &byte| {
                crc.rotate_left(5) ^ u32::from(byte)
            })
        }
    }

    /// Builds a frame whose trailer matches `TestCrc`.
    fn packet(payload: &[u8]) -> Vec<u8> {
        let mut bytes = PREAMBLE.to_vec();
        bytes.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        bytes.extend_from_slice(payload);
        bytes.extend_from_slice(&TestCrc.crc(&bytes).to_le_bytes());
        bytes
    }

    /// Exhausts every chunk, collecting payloads and checking stable buffer invariants.
    fn receive<const BUFFER: usize>(chunks: &[&[u8]]) -> Vec<Vec<u8>> {
        let mut rx = TransportRx::<BUFFER>::new();
        let mut crc = TestCrc;
        let mut payloads = Vec::new();
        for chunk in chunks {
            let mut parser = rx.process(chunk, &mut crc);
            while let Some(payload) = parser.next() {
                payloads.push(payload.to_vec());
            }
            // Stable state retains only an incomplete, structurally valid candidate.
            assert_eq!(rx.start, 0);
            assert!(rx.count <= BUFFER);
            if rx.count > 0 {
                assert_eq!(rx.buffer[0], PREAMBLE[0]);
                if rx.count >= 2 {
                    assert_eq!(rx.buffer[1], PREAMBLE[1]);
                }
                if rx.count >= PACKET_HEADER_LEN {
                    let total = usize::from(u16::from_le_bytes([rx.buffer[2], rx.buffer[3]]))
                        + PACKET_LEN_MIN;
                    assert!(total <= BUFFER);
                    assert!(rx.count < total);
                }
            }
        }
        payloads
    }

    /// Exercise preamble, length, payload, and trailer boundaries, including empty chunks.
    #[test]
    fn every_packet_split_and_minimum_buffer() {
        for payload in [&[][..], &[0x55, 0x99, 0, 255][..]] {
            let bytes = packet(payload);
            for split in 0..=bytes.len() {
                assert_eq!(
                    receive::<32>(&[&bytes[..split], &[], &bytes[split..]]),
                    vec![payload.to_vec()]
                );
            }
        }
        let bytes = packet(&[]);
        for split in 0..=bytes.len() {
            assert_eq!(
                receive::<8>(&[&bytes[..split], &bytes[split..]]),
                vec![vec![]]
            );
        }
    }

    /// Check direct and buffered extraction, including overlapping preamble starts.
    #[test]
    fn noise_multiple_packets_and_all_chunk_sizes() {
        let expected = vec![vec![1, 2, 3], vec![], vec![0x55, 0x55, 0x99, 7]];
        let mut stream = vec![0, 0x55, 0, 0x55];
        for payload in &expected {
            stream.extend(packet(payload));
            stream.extend([0, 0x55, 0, 3]);
        }
        for size in 1..=stream.len() {
            let chunks: Vec<_> = stream.chunks(size).collect();
            assert_eq!(receive::<64>(&chunks), expected, "chunk size {size}");
        }
    }

    /// The under-eight-byte shortcut must restore the header invariant before exhaustion.
    #[test]
    fn invalid_short_header_is_not_retained() {
        let mut rx = TransportRx::<8>::new();
        let mut crc = TestCrc;
        assert_eq!(rx.process(&[0x55], &mut crc).next(), None);
        assert_eq!(rx.process(&[0, 0, 0], &mut crc).next(), None);
        assert_eq!(rx.count, 0);
        // Oversized headers are also rejected when combined input is under eight bytes.
        assert_eq!(rx.process(&[0x55, 0x99], &mut crc).next(), None);
        assert_eq!(rx.process(&[1, 0], &mut crc).next(), None);
        assert_eq!(rx.count, 0);
    }

    /// Reject a completed bad header before copying any following frame into the buffer.
    #[test]
    fn invalid_header_does_not_copy_following_payload() {
        for (prefix, suffix) in [
            (&[0x55][..], &[0, 0, 0][..]),
            (&[0x55, 0x99][..], &[255, 255][..]),
        ] {
            let mut rx = TransportRx::<32>::new();
            let mut crc = TestCrc;
            assert_eq!(rx.process(prefix, &mut crc).next(), None);
            let mut incoming = suffix.to_vec();
            incoming.extend(packet(&[42; 16]));
            let mut parser = rx.process(&incoming, &mut crc);
            assert_eq!(parser.next(), Some(&[42; 16][..]));
            assert_eq!(parser.next(), None);
            assert!(rx.buffer[PACKET_HEADER_LEN..].iter().all(|&byte| byte == 0));
        }
    }

    /// Recover two valid inner frames from an outer candidate with a corrupt trailer.
    #[test]
    fn crc_failure_recovers_multiple_buffered_packets() {
        let mut inner = packet(&[1]);
        inner.extend(packet(&[2]));
        let mut stream = packet(&inner);
        *stream.last_mut().unwrap() ^= 1;
        for split in 0..=stream.len() {
            assert_eq!(
                receive::<64>(&[&stream[..split], &stream[split..]]),
                vec![vec![1], vec![2]]
            );
        }
    }

    /// Recover a real frame beginning inside a rejected candidate and ending beyond it.
    #[test]
    fn crc_failure_recovers_packet_spanning_buffer_and_input() {
        let mut stream = vec![0x55, 0x99, 8, 0];
        stream.extend([0; 6]);
        stream.extend(packet(&[42; 10]));
        stream.extend(packet(&[7]));
        for size in 1..=stream.len() {
            let chunks: Vec<_> = stream.chunks(size).collect();
            assert_eq!(receive::<32>(&chunks), vec![vec![42; 10], vec![7]]);
        }
    }

    /// Retained recovery bytes survive parser destruction and can be drained with empty input.
    #[test]
    fn recovery_survives_dropping_parser_after_yield() {
        let mut inner = packet(&[1]);
        inner.extend(packet(&[2]));
        let mut stream = packet(&inner);
        *stream.last_mut().unwrap() ^= 1;
        let mut rx = TransportRx::<64>::new();
        let mut crc = TestCrc;
        assert_eq!(rx.process(&stream[..1], &mut crc).next(), None);
        assert_eq!(rx.process(&stream[1..], &mut crc).next(), Some(&[1][..]));
        let mut parser = rx.process(&[], &mut crc);
        assert_eq!(parser.next(), Some(&[2][..]));
        assert_eq!(parser.next(), None);
    }

    /// Reject capacities unable to hold even an empty frame before parsing begins.
    #[test]
    #[should_panic(expected = "transport buffer must hold at least")]
    fn undersized_buffer_rejected_at_construction() {
        TransportRx::<7>::new();
    }
}

extern crate std;

use super::*;
use std::{vec, vec::Vec};

use crate::SoftwareCrc;

/// Records checksum inputs while delegating calculations to `SoftwareCrc`.
#[derive(Default)]
struct RecordingCrc {
    input: Vec<u8>,
    calls: usize,
}

impl CrcDevice for RecordingCrc {
    fn crc(&mut self, data: &[u8]) -> u32 {
        self.input.extend_from_slice(data);
        self.calls += 1;
        SoftwareCrc.crc(data)
    }
}

/// Builds a frame whose trailer matches `SoftwareCrc`.
fn packet(payload: &[u8]) -> Vec<u8> {
    let mut bytes = PREAMBLE.to_vec();
    bytes.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    bytes.extend_from_slice(payload);
    bytes.extend_from_slice(&SoftwareCrc.crc(&bytes).to_le_bytes());
    bytes
}

/// Exhausts every chunk, collecting payloads and checking stable buffer invariants.
fn receive<const BUFFER: usize>(chunks: &[&[u8]]) -> Vec<Vec<u8>> {
    let mut rx = TransportRx::<BUFFER>::new();
    let mut crc = SoftwareCrc;
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
                let total =
                    usize::from(u16::from_le_bytes([rx.buffer[2], rx.buffer[3]])) + PACKET_LEN_MIN;
                assert!(total <= BUFFER);
                assert!(rx.count < total);
            }
        }
    }
    payloads
}

/// Finalization must pass exactly the header and written payload to CRC.
#[test]
fn tx_wire_bytes_and_in_place_payload() {
    let mut tx = TransportTx::<32>::new();
    let payload = tx.payload_mut();
    payload[..3].copy_from_slice(&[1, 0x55, 0x99]);
    let payload_ptr = payload.as_ptr();
    let mut crc = RecordingCrc::default();
    let frame = tx.finish(3, &mut crc).unwrap();
    assert_eq!(
        frame,
        &[0x55, 0x99, 3, 0, 1, 0x55, 0x99, 0xF3, 0xCC, 0x95, 0xDE]
    );
    assert_eq!(frame[PACKET_HEADER_LEN..].as_ptr(), payload_ptr);
    // The packet's borrow does not keep the CRC device borrowed.
    assert_eq!(crc.input, frame[..7]);
}

#[test]
fn tx_capacity_limits_and_empty_packet() {
    let mut tx = TransportTx::<8>::default();
    assert_eq!(tx.payload_capacity(), 0);
    assert!(tx.payload_mut().is_empty());
    assert_eq!(tx.finish(0, &mut SoftwareCrc).unwrap(), packet(&[]));

    let mut default_tx = TransportTx::<512>::default();
    assert_eq!(default_tx.payload_capacity(), 504);
    assert_eq!(default_tx.payload_mut().len(), 504);

    // Extra buffer space must not expose payload lengths beyond u16::MAX.
    let mut tx = TransportTx::<65544>::new();
    assert_eq!(tx.payload_capacity(), 65535);
    let payload = tx.payload_mut();
    assert_eq!(payload.len(), 65535);
    payload.fill(0xA5);
    let frame = tx.finish(65535, &mut SoftwareCrc).unwrap();
    assert_eq!(frame.len(), 65543);
    assert_eq!(&frame[..4], &[0x55, 0x99, 0xFF, 0xFF]);
    let mut rx = TransportRx::<65543>::new();
    let mut crc = SoftwareCrc;
    let mut parser = rx.process(frame, &mut crc);
    let received = parser.next().unwrap();
    assert_eq!(received.len(), 65535);
    assert!(received.iter().all(|&byte| byte == 0xA5));
    assert_eq!(parser.next(), None);
}

/// Invalid lengths must fail before any header update or checksum access.
#[test]
fn tx_oversized_length_preserves_storage() {
    let mut tx = TransportTx::<16>::new();
    tx.payload_mut().fill(7);
    tx.finish(8, &mut SoftwareCrc).unwrap();
    let before = tx.buffer;
    let mut crc = RecordingCrc::default();
    for len in [9, 65536, usize::MAX] {
        assert_eq!(tx.finish(len, &mut crc), Err(TxError::PayloadTooLarge));
        assert_eq!(tx.buffer, before);
        assert_eq!(crc.calls, 0);
    }

    let mut tx = TransportTx::<65544>::new();
    assert_eq!(tx.finish(65536, &mut crc), Err(TxError::PayloadTooLarge));
    assert_eq!(crc.calls, 0);
}

/// Reuse one transmitter for long, short, empty, and full-size packets.
#[test]
fn tx_reuse_chained_packets_and_all_chunk_boundaries() {
    let expected = vec![vec![42; 16], vec![0x55, 0x99], vec![], vec![7; 24]];
    let mut tx = TransportTx::<32>::new();
    let mut crc = SoftwareCrc;
    let mut stream = Vec::new();
    for payload in &expected {
        tx.payload_mut()[..payload.len()].copy_from_slice(payload);
        let frame = tx.finish(payload.len(), &mut crc).unwrap();
        assert_eq!(frame, packet(payload));
        // Copy only to retain a test stream; production can send each slice directly.
        stream.extend_from_slice(frame);
    }
    for split in 0..=stream.len() {
        assert_eq!(
            receive::<32>(&[&stream[..split], &[], &stream[split..]]),
            expected,
            "split {split}"
        );
    }
    for size in 1..=stream.len() {
        let chunks: Vec<_> = stream.chunks(size).collect();
        assert_eq!(receive::<32>(&chunks), expected, "chunk size {size}");
    }
}

#[test]
fn tx_software_crc_known_frame() {
    let mut tx = TransportTx::<32>::new();
    tx.payload_mut()[..9].copy_from_slice(b"123456789");
    let mut crc = SoftwareCrc;
    let frame = tx.finish(9, &mut crc).unwrap();
    // Known CRC-32/ISO-HDLC vectors, including the header in the frame checksum.
    assert_eq!(crc.crc(b"123456789"), 0xCBF4_3926);
    assert_eq!(&frame[13..], &0xBC17_6960u32.to_le_bytes());
    let mut rx = TransportRx::<32>::new();
    let mut parser = rx.process(frame, &mut crc);
    assert_eq!(parser.next(), Some(&b"123456789"[..]));
    assert_eq!(parser.next(), None);
}

#[test]
#[should_panic(expected = "transport buffer must hold at least")]
fn tx_undersized_buffer_rejected_at_construction() {
    TransportTx::<7>::new();
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
    let mut crc = SoftwareCrc;
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
        let mut crc = SoftwareCrc;
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
    let mut crc = SoftwareCrc;
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

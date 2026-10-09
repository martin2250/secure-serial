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

/// Reproducible end-to-end stress test of framing, corruption recovery, and chunking.
#[test]
fn large_noisy_stream_recovers_all_uncorrupted_packets() {
    const BUFFER: usize = 64;
    const PACKETS: usize = 4096;
    const MAX_PAYLOAD: usize = BUFFER - PACKET_LEN_MIN;

    // Xorshift64 keeps the test reproducible without adding a random dependency.
    fn random(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    fn random_below(state: &mut u64, limit: usize) -> usize {
        (random(state) % limit as u64) as usize
    }

    let mut rng = 0x4D59_5DF4_D0F3_3173;
    // Only 57 lengths fit this buffer; cycle through all of them, including empty
    // and maximum-size payloads, rather than requiring 4096 distinct lengths.
    let packets: Vec<Vec<u8>> = (0..PACKETS)
        .map(|index| {
            (0..index % (MAX_PAYLOAD + 1))
                .map(|_| random(&mut rng) as u8)
                .collect()
        })
        .collect();
    let mut expected = Vec::new();
    let mut stream = Vec::new();
    let mut tx = TransportTx::<BUFFER>::new();
    let mut crc = SoftwareCrc;
    let mut corruptions = [0; 8];

    for (index, payload) in packets.into_iter().enumerate() {
        tx.payload_mut()[..payload.len()].copy_from_slice(&payload);
        let mut frame = tx.finish(payload.len(), &mut crc).unwrap().to_vec();
        if index % 8 == 0 {
            let corruption = (index / 8) % corruptions.len();
            corruptions[corruption] += 1;
            let crc_start = PACKET_HEADER_LEN + payload.len();
            match corruption {
                0 => frame[0] ^= 1, // Damaged preamble.
                1 => frame[2..4].copy_from_slice(&u16::MAX.to_le_bytes()),
                2 => {
                    // Shorten the declared length, or use a false maximum for empty data.
                    frame[2] = if payload.is_empty() {
                        MAX_PAYLOAD as u8
                    } else {
                        (payload.len() - 1) as u8
                    };
                }
                3 => frame[2] = (payload.len() + 1) as u8,
                4 => {
                    // Empty payloads have no data byte to flip; damage their CRC.
                    let offset = if payload.is_empty() {
                        crc_start
                    } else {
                        PACKET_HEADER_LEN + random_below(&mut rng, payload.len())
                    };
                    frame[offset] ^= 0x80;
                }
                5 => frame[crc_start + 2] ^= 1,
                6 => {
                    frame.remove(PACKET_HEADER_LEN); // Deleted payload/CRC byte.
                }
                7 => frame.insert(PACKET_HEADER_LEN, random(&mut rng) as u8),
                _ => unreachable!(),
            }
        } else {
            expected.push(payload);
        }
        stream.extend_from_slice(&frame);
        let noise_len = random_below(&mut rng, 17);
        stream.extend((0..noise_len).map(|_| random(&mut rng) as u8));
        if index % 64 == 0 {
            // A plausible false header forces CRC recovery across later packets.
            stream.extend_from_slice(&[PREAMBLE[0], PREAMBLE[1], MAX_PAYLOAD as u8, 0]);
        }
    }
    assert_eq!(corruptions, [64; 8]);
    assert_eq!(expected.len(), PACKETS - 512);
    // Complete any false candidate near EOF so it cannot hide the last packet.
    stream.extend_from_slice(&[0; BUFFER]);

    #[derive(Debug, Clone, Copy)]
    enum ChunkSizes {
        Fixed(usize),
        Uniform(usize),
        Bursty,
    }
    let distributions = [
        ChunkSizes::Fixed(1),
        ChunkSizes::Fixed(2),
        ChunkSizes::Fixed(3),
        ChunkSizes::Fixed(7),
        ChunkSizes::Fixed(8),
        ChunkSizes::Fixed(31),
        ChunkSizes::Fixed(63),
        ChunkSizes::Fixed(64),
        ChunkSizes::Fixed(65),
        ChunkSizes::Fixed(127),
        ChunkSizes::Fixed(1024),
        ChunkSizes::Fixed(stream.len()),
        ChunkSizes::Uniform(4),
        ChunkSizes::Uniform(BUFFER),
        ChunkSizes::Uniform(256),
        ChunkSizes::Uniform(4096),
        ChunkSizes::Bursty,
    ];
    for distribution in distributions {
        for seed in [
            0x1234_5678_9ABC_DEF0,
            0xCAFE_BABE_0123_4567,
            0xF00D_9876_5432_10AB,
        ] {
            let mut chunk_rng = seed;
            let mut chunks = Vec::new();
            let mut offset = 0;
            while offset < stream.len() {
                let size = match distribution {
                    ChunkSizes::Fixed(size) => size,
                    ChunkSizes::Uniform(max) => 1 + random_below(&mut chunk_rng, max),
                    ChunkSizes::Bursty => {
                        if random(&mut chunk_rng) & 1 == 0 {
                            1 + random_below(&mut chunk_rng, 4)
                        } else {
                            128 + random_below(&mut chunk_rng, 3969)
                        }
                    }
                };
                let end = (offset + size).min(stream.len());
                chunks.push(&stream[offset..end]);
                // Empty reads also occur between independently sized chunks.
                if random(&mut chunk_rng).is_multiple_of(16) {
                    chunks.push(&[]);
                }
                offset = end;
            }
            let received = receive::<BUFFER>(&chunks);
            assert_eq!(
                received.len(),
                expected.len(),
                "{distribution:?}, seed {seed:#x}"
            );
            for (index, (actual, wanted)) in received.iter().zip(&expected).enumerate() {
                assert_eq!(
                    actual, wanted,
                    "{distribution:?}, seed {seed:#x}, packet {index}"
                );
            }
        }
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

/// Reject oversized lengths, including those whose frame size exceeds u16::MAX.
#[test]
fn oversized_lengths_recover_at_all_chunk_boundaries() {
    for len in core::iter::once(9u16).chain((u16::MAX - 7)..=u16::MAX) {
        let mut stream = PREAMBLE.to_vec();
        stream.extend_from_slice(&len.to_le_bytes());
        stream.extend(packet(b"OK"));
        for split in 0..=stream.len() {
            assert_eq!(
                receive::<16>(&[&stream[..split], &stream[split..]]),
                vec![b"OK".to_vec()],
                "length {len}, split {split}"
            );
        }
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

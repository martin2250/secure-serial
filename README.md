# secure-serial

A `no_std` receiver for CRC-framed packets arriving over a byte stream in arbitrary chunks. `TransportRx` retains incomplete candidates in a fixed buffer and yields validated payload slices. It accepts noise between packets and searches again after invalid headers or CRCs.

The current public API consists of `TransportRx`, `IterRx`, and `CrcDevice`. Reading bytes from UART or USB, sending packets, acknowledgements, retransmission, and assembling application messages are responsibilities of the caller.

## Wire format

| Offset | Size | Contents |
| --- | --- | --- |
| 0 | 2 bytes | Preamble: `0x55 0x99` |
| 2 | 2 bytes | Payload length, unsigned little-endian `u16` |
| 4 | Declared length | Payload |
| 4 + length | 4 bytes | CRC, unsigned little-endian `u32` |

The CRC covers the preamble, length bytes, and payload, excluding the CRC trailer. Both peers must agree on the CRC algorithm and its parameters. The length describes only the payload, so a complete frame occupies `length + 8` bytes. Empty payloads are valid and yield `Some(&[])`.

`TransportRx::<BUFFER>::new()` and `Default::default()` require `BUFFER >= 8` and panic otherwise. The capacity limits the entire frame, even when it arrives in a single chunk. Maximum accepted payload length is `min(BUFFER - 8, 65535)`; the default capacity of 512 supports 504-byte payloads. Incoming chunks can be larger than the buffer and contain many frames.

## Receiving packets

Keep one receiver across successive chunks. For each chunk, call `process()` and exhaust the returned parser with `while let Some(payload) = parser.next()`. Handle or copy each payload before requesting the next one.

This example supplies a CRC-32/ISO-HDLC implementation, constructs a frame containing `OK`, and receives it across three chunks:

```rust
use secure_serial::{CrcDevice, TransportRx};

struct Crc32;

impl CrcDevice for Crc32 {
    fn crc(&mut self, data: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &byte in data {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
}

let mut crc = Crc32;
let mut frame = [0x55, 0x99, 2, 0, b'O', b'K', 0, 0, 0, 0];
let checksum = crc.crc(&frame[..6]);
frame[6..].copy_from_slice(&checksum.to_le_bytes());

let mut receiver = TransportRx::<32>::new();
let mut received = 0;
for chunk in [&frame[..1], &frame[1..5], &frame[5..]] {
    let mut parser = receiver.process(chunk, &mut crc);
    while let Some(payload) = parser.next() {
        assert_eq!(payload, b"OK");
        received += 1;
    }
}
assert_eq!(received, 1);
```

`process()` is lazy: creating a parser does not consume or copy bytes. `next()` returns `None` when the chunk is exhausted; an incomplete candidate is retained for the next chunk. Further calls on that exhausted parser also return `None`. An empty chunk may still yield packets left in the buffer during recovery.

Exhaust the parser before dropping it. Dropping it early loses its remaining uncopied input. Bytes already retained in the receiver survive, including recovery bytes that have not yet been yielded.

## Payload borrowing

`IterRx` provides a lending `next(&mut self) -> Option<&[u8]>` method and does not implement `Iterator`. A payload wholly available in incoming data is returned directly from that slice. A packet spanning chunks is assembled in the receiver's buffer and returned from there. Neither path allocates heap storage.

Each payload borrows the parser. Release that borrow before calling `next()` again or dropping the parser, and drop the parser before reusing the receiver. To retain several payloads, copy them into caller-owned storage. Use a `while let` loop; iterator adapters such as `collect()` and `for` iteration are unavailable. Rust enforces this lifetime so buffer reuse cannot overwrite a payload still in use.

## Continuation and recovery

Both incoming data and buffered bytes use the same scanner. It searches for `55`, checks `99` when available, accepts only lengths within capacity, and validates complete candidates against their CRC trailer. An invalid candidate resumes the search one byte after its start, preserving overlapping preambles.

When continuing a buffered candidate:

1. If buffered and incoming bytes together occupy fewer than eight bytes, copy all available input and rescan before waiting. An invalid header is therefore discarded even on this short-input path.
2. Otherwise, complete only the four-byte header first, if necessary. Validate the preamble and length before copying payload bytes. Invalid headers leave the rest of the incoming data available for searching.
3. With a valid header, copy only enough bytes to complete its declared frame, or all available input if it is shorter. Following frames remain in the incoming slice.
4. Validate the completed frame's CRC. On failure, rescan its copied bytes as well as the remaining input. Valid packets may be embedded within a rejected candidate or span its end and the untouched input.

After `next()` returns `None`, retained data is empty or consists of one incomplete candidate at buffer offset zero. If present, its first byte is `55`; an available second byte is `99`; a complete header declares a frame within capacity; and the retained byte count is smaller than that declared frame. Its CRC remains unverified.

This is a guarantee about a candidate, not proof that it is an actual valid packet. A plausible false length can cause the parser to wait for the entire declared frame before checking its CRC. There is no timeout or automatic expiration.

During recovery, copied bytes may temporarily contain several complete or partial packets. After yielding a buffered payload, the receiver tracks the unconsumed suffix by offset and defers compaction until the next call, after the payload borrow ends. The single incomplete-candidate invariant applies when the parser is exhausted, rather than after every yielded payload.

## CRC devices and features

Implement `CrcDevice` for a software or hardware checksum provider. Each `crc(data)` call must calculate an independent checksum over the entire supplied slice. Reset a hardware peripheral for every call. Recovery may check several overlapping candidates, so results must not depend on previous calls.

The default feature set is empty. `software-crc` enables the `crc` dependency and an internal CRC-32/ISO-HDLC adapter; that adapter is currently private. `defmt` enables its dependency; the current receiver does not emit log messages. The example above works with the default features.

## Verification

The transport tests cover every split position for empty and nonempty frames, the minimum buffer capacity, varying chunk sizes, noise, overlapping preambles, invalid and oversized headers, and header rejection before copying following payloads. They also cover CRC recovery of nested packets, recovered packets spanning the buffer and incoming input, and buffered recovery after dropping a parser. The helper checks the retained-buffer invariant after every exhausted chunk.

A runnable documentation example verifies receiving across chunks. A compile-fail example verifies that callers cannot reuse the parser while retaining an earlier payload borrow.

```sh
cargo test --lib --all-features
cargo test --doc
cargo clippy --lib --tests --all-features -- -D warnings
cargo fmt --all -- --check
```

## Toolchain and license

The crate uses Rust edition 2024 and declares Rust 1.85 as its minimum version.

Licensed under either Apache-2.0 or MIT, at your option. Unless explicitly stated otherwise, contributions are dual licensed under these terms.

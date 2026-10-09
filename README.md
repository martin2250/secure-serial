# secure-serial

A `no_std` transport for CRC-framed packets over a byte stream. `TransportTx` builds packets in reusable storage without copying the payload. `TransportRx` retains incomplete candidates in a fixed buffer and yields validated payload slices from arbitrary incoming chunks. It accepts noise between packets and searches again after invalid headers or CRCs.

The public API consists of `TransportTx`, `TxError`, `TransportRx`, `IterRx`, and `CrcDevice`, plus `SoftwareCrc` when the `software-crc` feature is enabled. Reading and writing bytes through UART or USB, acknowledgements, retransmission, and assembling application messages are responsibilities of the caller.

## Wire format

| Offset | Size | Contents |
| --- | --- | --- |
| 0 | 2 bytes | Preamble: `0x55 0x99` |
| 2 | 2 bytes | Payload length, unsigned little-endian `u16` |
| 4 | Declared length | Payload |
| 4 + length | 4 bytes | CRC, unsigned little-endian `u32` |

The CRC covers the preamble, length bytes, and payload, excluding the CRC trailer. Both peers must agree on the CRC algorithm and its parameters. The length describes only the payload, so a complete frame occupies `length + 8` bytes. Empty payloads are valid and yield `Some(&[])`.

Both `TransportTx::<BUFFER>` and `TransportRx::<BUFFER>` require `BUFFER >= 8` at construction and panic otherwise, including through `Default::default()`. The capacity limits the entire frame, even when it arrives in a single chunk. Maximum payload length is `min(BUFFER - 8, 65535)`; the default capacity of 512 supports 504-byte payloads. Incoming chunks can be larger than the receiver buffer and contain many frames.

## Sending packets

Create one `TransportTx` and reuse it. `payload_mut()` exposes only the payload region, reserving room for the header and CRC. Write your payload directly into this slice, then call `finish(payload_len, &mut crc)` with the number of bytes written. It adds the header and CRC in place and returns the complete packet as a read-only slice. Empty payloads are supported.

Enable the `software-crc` feature as shown under [CRC devices and features](#crc-devices-and-features). This example uses `SoftwareCrc` and sends two packets directly to a receiver, without copying the payload between buffers:

```rust
# #[cfg(feature = "software-crc")]
# {
use secure_serial::{SoftwareCrc, TransportRx, TransportTx};

let mut tx = TransportTx::<32>::new();
let mut rx = TransportRx::<32>::new();
let mut crc = SoftwareCrc;
assert_eq!(tx.payload_capacity(), 24);

for value in [1u16, 2] {
    // Encode the application payload directly into transmit storage.
    tx.payload_mut()[..2].copy_from_slice(&value.to_le_bytes());
    let packet = tx.finish(2, &mut crc).unwrap();
    // In an application, write this entire slice to UART or USB here.
    let mut parser = rx.process(packet, &mut crc);
    assert_eq!(parser.next(), Some(&value.to_le_bytes()[..]));
    assert_eq!(parser.next(), None);
}
# }
```

`finish()` returns `TxError::PayloadTooLarge` for a length exceeding the buffer or wire-format limit. On error it leaves storage unchanged and does not invoke the CRC device. The returned packet borrows only the transmitter; the CRC device can be reused immediately.

Storage is initialized once and is not cleared between packets. Write every payload byte included in the length passed to `finish()`; unused bytes are excluded from the returned packet. The CRC follows the actual payload, rather than occupying a fixed position at the end of the buffer. Finalization does not move or copy payload bytes and allocates no heap storage.

Send each packet completely before reusing the transmitter, including waiting for an asynchronous write to finish. Rust prevents modifying the transmitter while its packet slice is still in use. Consecutive packets need no separator: write them one after another to form a stream that `TransportRx` can parse across any chunk boundaries. One transmitter holds one packet at a time; retaining several packets concurrently requires additional storage.

## Receiving packets

Keep one receiver across successive chunks. For each chunk, call `process()` and exhaust the returned parser with `while let Some(payload) = parser.next()`. Handle or copy each payload before requesting the next one.

This example uses `SoftwareCrc` with the `software-crc` feature enabled, constructs a frame containing `OK`, and receives it across three chunks:

```rust
# #[cfg(feature = "software-crc")]
# {
use secure_serial::{CrcDevice, SoftwareCrc, TransportRx};

let mut crc = SoftwareCrc;
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
# }
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

The default feature set is empty. Enable `software-crc` to use the public `SoftwareCrc` adapter, which implements `CrcDevice` using CRC-32/ISO-HDLC from the `crc` dependency:

```toml
[dependencies]
secure-serial = { version = "1.0", features = ["software-crc"] }
```

```rust
# #[cfg(feature = "software-crc")]
# {
use secure_serial::{CrcDevice, SoftwareCrc};

let mut crc = SoftwareCrc;
assert_eq!(crc.crc(b"123456789"), 0xCBF4_3926);
# }
```

Pass `&mut crc` to `TransportTx::finish()` or `TransportRx::process()`.

Run the example in `examples/simple.rs` with `cargo run --example simple --features software-crc` to send a packet and receive it across several chunks.

`defmt` enables its dependency; the current receiver does not emit log messages. The sending and receiving examples above require `software-crc`; hardware CRC providers can implement `CrcDevice` with the default features.

## Verification

Unit tests live in `tests/unit/transport.rs` and are included by the transport module so they can verify private buffer invariants.

The transport tests cover every split position for empty and nonempty frames, the minimum buffer capacity, varying chunk sizes, noise, overlapping preambles, invalid and oversized headers, and header rejection before copying following payloads. They also cover CRC recovery of nested packets, recovered packets spanning the buffer and incoming input, and buffered recovery after dropping a parser. The helper checks the retained-buffer invariant after every exhausted chunk.

A runnable documentation example verifies receiving across chunks. A compile-fail example verifies that callers cannot reuse the parser while retaining an earlier payload borrow.

Transmit tests verify exact wire bytes and CRC coverage, payload storage reuse, minimum and maximum capacities, length rejection without storage changes or CRC calls, and chained Tx-to-Rx round trips across every split and chunk size. All transport tests use `SoftwareCrc`, including known CRC-32/ISO-HDLC vectors to verify the CRC trailer. Test spies delegate checksum calculations to `SoftwareCrc` and record calls to verify CRC coverage and length rejection. Documentation examples also verify transmitter reuse and prevent modifying a transmitter while its packet slice remains in use.

```sh
cargo test --lib --all-features
cargo test --doc --all-features
cargo clippy --lib --tests --all-features -- -D warnings
cargo fmt --all -- --check
```

## Toolchain and license

The crate uses Rust edition 2024 and declares Rust 1.85 as its minimum version.

Licensed under either Apache-2.0 or MIT, at your option. Unless explicitly stated otherwise, contributions are dual licensed under these terms.

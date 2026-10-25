# secure-serial

A `no_std`, allocation-free transport for CRC-framed packets over a byte stream.
`TransportTx` builds frames in reusable storage without copying the payload;
`TransportRx` parses arbitrary chunks, retains incomplete frames, and recovers
from noise, invalid headers, and CRC failures. UART/USB I/O, acknowledgements,
retransmission, and application message assembly are the caller's responsibility.

CRC detects accidental corruption. It provides no authentication, encryption,
or replay protection.

**Version 1.0 scope:** The previously published
[version 0.1.0](https://crates.io/crates/secure-serial/0.1.0) included higher-level
packet reassembly. Version 1.0 currently contains only the transport layer and
does not include packet reassembly. Higher-level functions are planned as a
second layer on top of the transport.

## Quick start

The default feature set is empty. Enable `software-crc` for the built-in
CRC-32/ISO-HDLC provider, or implement `CrcDevice` for your hardware or software CRC.

```toml
[dependencies]
secure-serial = { version = "1.0", features = ["software-crc"] }
```

```rust
# #[cfg(feature = "software-crc")]
# {
use secure_serial::{SoftwareCrc, TransportRx, TransportTx};

let mut tx = TransportTx::<32>::new();
let mut rx = TransportRx::<32>::new();
let mut crc = SoftwareCrc;

// Write directly into transmit storage and finalize the frame in place.
tx.payload_mut()[..2].copy_from_slice(b"OK");
let packet = tx.finish(2, &mut crc).unwrap();

// Send the complete packet over UART/USB; receive it in arbitrary chunks.
for chunk in packet.chunks(3) {
    let mut parser = rx.process(chunk, &mut crc);
    while let Some(payload) = parser.next() {
        assert_eq!(payload, b"OK");
    }
}
# }
```

Run the standalone example with
`cargo run --example simple --features software-crc`.

## Embedded diagnostics

Enable the optional `defmt` feature to log packet preparation and reception,
transmit lengths exceeding capacity, rejected receive lengths, invalid preambles,
CRC mismatches, and bytes discarded while searching for a frame. Messages use
the debug level and include lengths and checksum values, without payload contents.
`TxError` also implements `defmt::Format` with this feature enabled.

```toml
[dependencies]
secure-serial = { version = "1.0", features = ["software-crc", "defmt"] }
```

The firmware must provide a defmt logger (for example, `defmt-rtt`) and its
[linker setup](https://defmt.ferrous-systems.com/setup). Build with
`DEFMT_LOG=secure_serial=debug` to include these messages; see defmt's
[filtering documentation](https://defmt.ferrous-systems.com/filtering).
Without the feature, the library has no logging dependency or logging code.

## Wire format and capacity

| Size | Contents |
| --- | --- |
| 2 bytes | Preamble: `0x55 0x99` |
| 2 bytes | Payload length, little-endian `u16` |
| Declared length | Payload |
| 4 bytes | CRC, little-endian `u32` |

The CRC covers the preamble, length, and payload. Both peers must use the same
CRC algorithm and parameters. Empty payloads are valid and yield `Some(&[])`.

`TransportTx::<BUFFER>` and `TransportRx::<BUFFER>` require `BUFFER >= 8` and
panic on construction otherwise. Capacity includes the entire frame: maximum
payload length is `min(BUFFER - 8, 65535)`. The default 512-byte buffer supports
504-byte payloads. Incoming chunks may exceed the receiver's capacity and contain
multiple frames.

Choose `BUFFER` for small transport frames, well below `u16::MAX`, rather than
for an entire large application message. A future higher-level reassembly layer
is planned to assemble larger messages from multiple transport frames.

## Usage rules

- Reuse one transmitter and receiver. Write every payload byte included in the
  length passed to `finish()`; storage is not cleared between packets.
  Oversized lengths return `TxError::PayloadTooLarge` without modifying storage
  or calling the CRC provider.
- Send the entire returned packet before reusing the transmitter, including
  waiting for asynchronous writes. Consecutive frames need no separator.
- Exhaust each parser before dropping it: unconsumed incoming bytes are lost
  when it is dropped. Incomplete buffered candidates survive for the next chunk.
- `IterRx` has a lending `next()` method, not an `Iterator` implementation.
  Handle or copy each payload before calling `next()` again. Payloads borrow
  either the input chunk or the receiver's buffer; retaining them requires
  caller-owned storage.
- Parsing is lazy. CRC failures and invalid headers resume scanning one byte
  after the candidate's start, preserving overlapping or embedded frames.
  A plausible false length can delay recovery until the full candidate arrives;
  there is no timeout or automatic expiration.
- Each `CrcDevice::crc(data)` call must compute an independent checksum over the
  supplied slice. Reset hardware CRC peripherals on every call; recovery may
  check multiple overlapping candidates.

See the [API documentation](https://docs.rs/secure-serial) for
method details and borrowing guarantees.

## Development

Tests cover framing, chunk boundaries, buffer limits, noise and CRC recovery,
transmitter reuse, and borrowing guarantees.

```sh
cargo test --lib --all-features
cargo test --doc --all-features
cargo clippy --lib --tests --all-features -- -D warnings
cargo fmt --all -- --check
```

Requires Rust 1.89 or later (edition 2024). Licensed under MIT or Apache-2.0,
at your option; contributions use the same dual license.

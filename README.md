# secure-serial

Reliable framed messaging over a byte-oriented serial link: **magic + length + type**, **CRC-32**, and **per-chunk acknowledgements** with **sender retransmission**. Built for **`#![no_std]`** firmware using **Embassy** async (`embassy-time`, `embassy-sync`, `embassy-futures`), with bounded buffers via **`heapless`** and **`embedded-buffer-pool`**.

## When to use this crate

Reach for `secure-serial` when you have a **raw serial stream** (UART, USB-CDC, etc.) and you need:

- **Detection of corrupted data** (CRC on each frame)
- **Larger application messages** than one frame, without allocating unbounded buffers on the device
- **Positive acknowledgement** of each chunk so the sender can **retry** after loss or noise

## Features

- **`no_std`**-first; integrates with **Embassy** timers and channels
- **Chunked packets**: up to **128 bytes** of payload per wire chunk (`CHUNK_PAYLOAD_MAX`); total packet size is limited by your RX reassembly buffer size
- **Separate async tasks** for reading (`run_read`) and writing (`run_write`), plus an application-facing **`SecureSerialSender`** for outgoing packets
- **Pluggable transport**: implement `TransportRead`, `TransportWrite`, and `CrcDevice` for your HAL or host shim
- Optional **`defmt`** logging in the receiver path (enable crate feature `defmt`)

## Wire format (summary)

Frames start with the magic bytes **`0x5E 0x5E`**, followed by a **length** byte, **packet type** (`DATA` or `ACK`), and type-specific payload. A **little-endian CRC-32** is appended; the CRC covers the frame bytes **up to but not including** that trailer.

`DATA` chunks carry a **packet id**, **total packet length**, **chunk index**, and **payload slice**. The receiver reassembles chunks into a contiguous buffer and forwards the completed packet to your code. `ACK` frames carry one or more **`Ack`** records so the peer’s `SecureSerialSender` can advance its window.

Constants and the `Ack` helper live in the **`protocol`** module; see `CHUNK_LEN_MAX` for the maximum wire size of one chunk.

## Architecture

| Piece | Responsibility |
|-------|----------------|
| **`SecureSerialResources`** | Fixed **TX chunk pool**, **RX reassembly pool**, and **channels** linking the I/O tasks to the app (including ACK paths). Use **`SecureSerialResourcesDefault`** for typical sizes unless you tune `N_INFLIGHT`, `N_RX_POOL`, and `N_BUF`. |
| **`SecureSerialSender`** | **`write_packet`**: splits a slice into chunks, **waits for TX buffers**, **queues** frames for `run_write`, **consumes ACKs** from the peer, and **retransmits** according to delay and retry limits. |
| **`run_read`** | Scans incoming bytes, validates frames, **reassembles** `DATA`, **forwards ACKs** toward the local sender, **queues completed packets** for the application. |
| **`run_write`** | **Multiplexes** pending `DATA` chunks and outgoing `ACK`s onto the transport and appends CRC. |

Place **`SecureSerialResources`** in a **`'static`** slot (for example **`static_cell::StaticCell`**) so pools and channel endpoints can be shared across Embassy tasks.

## Example (wiring)

Spawn **`run_write`** and **`run_read`** with your transport and CRC implementation; connect **`SecureSerialSender::new`** to the same resource bundle’s **`tx_pool`**, **`tx_chunks_sender`**, and **`acks_from_peer_receiver`**. Receive completed packets from **`rx_complete_receiver`**.

A runnable **host-side** sketch using Tokio and pipes lives in **`examples/simple.rs`** (dev-dependencies only).

```text
Conceptual wiring — see examples/simple.rs for a full loopback demo.

  static RES: StaticCell<SecureSerialResourcesDefault<MyMutex>> = StaticCell::new();
  let res: &'static _ = RES.init(SecureSerialResourcesDefault::new());

  spawner.spawn(run_write(uart_tx, res.tx_chunks_receiver(), res.acks_to_wire_receiver(), crc));
  spawner.spawn(run_read(uart_rx, crc, res.rx_pool(), res.rx_complete_sender(),
      res.acks_to_wire_sender(), res.acks_from_peer_sender()));

  let mut sender = SecureSerialSender::new(
      res.tx_pool(),
      res.tx_chunks_sender(),
      res.acks_from_peer_receiver(),
      Duration::from_millis(50),
      3,
  );
  sender.write_packet(b"hello").await?;
```

## Crate layout

- **`protocol`** — Magic bytes, chunk limits, **`Ack`**
- **`transport`** — **`TransportRead`**, **`TransportWrite`**, **`CrcDevice`**
- **`resources`** — **`SecureSerialResources`**
- **`sender`** — **`SecureSerialSender`**
- **`receiver`** — **`run_read`**, **`run_write`**

## Dependencies

`embassy-futures`, `embassy-sync`, `embassy-time`, `heapless`, `embedded-buffer-pool`.

Enable the `defmt` feature if you want receiver-side diagnostics on embedded targets.

## MSRV / edition

This crate uses the **2024** edition (**Rust 1.85+**); use a matching **stable** or **nightly** toolchain.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this crate by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.

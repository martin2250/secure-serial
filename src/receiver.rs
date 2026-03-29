//! Inbound framing and reassembly ([`run_read`]) and outbound multiplexing ([`run_write`]).
//!
//! These are the long-running tasks that talk to [`crate::TransportRead`] / [`crate::TransportWrite`]
//! and the channels inside [`crate::SecureSerialResources`].

use embassy_futures::select::{Either, select};
use embassy_sync::{blocking_mutex::raw::RawMutex, channel};
use embedded_buffer_pool::{BufferGuard, BufferPool, MappedBufferGuard};
use heapless::Vec;

use crate::protocol::{
    Ack, CHUNK_LEN_MAX, CHUNK_PAYLOAD_MAX, MAGIC, MAGIC_0, MAGIC_1, PACKET_ACK, PACKET_DATA,
};
use crate::transport::{CrcDevice, TransportRead, TransportWrite};

struct RxPacket<M: RawMutex + 'static, const N_BUF: usize> {
    buffer: BufferGuard<M, [u8; N_BUF]>,
    packet_id: u16,
    packet_length: usize,
    buffer_written: [u32; 4], // support at most 128 chunks / 16k bytes
    buffer_written_count: usize,
}

/// Reads bytes from `transport`, validates CRC, reassembles `DATA` packets into `rx_pool` buffers,
/// forwards `ACK` records to `acks_received`, and sends local `ACK`s on `acks_to_send`.
///
/// Completed packets are sent on `rx_queue` as a mapped guard over the received length.
pub async fn run_read<
    M: RawMutex + 'static,
    T: TransportRead,
    const N_INFLIGHT: usize,
    const N_POOL: usize,
    const N_BUF: usize,
>(
    transport: &mut T,
    crc_dev: &mut impl CrcDevice,
    buffer_pool: &'static BufferPool<M, [u8; N_BUF], N_POOL>,
    rx_queue: channel::Sender<'_, M, MappedBufferGuard<M, [u8]>, N_POOL>,
    acks_to_send: channel::Sender<'_, M, Ack, N_INFLIGHT>,
    acks_received: channel::Sender<'_, M, Ack, N_INFLIGHT>,
) -> Result<(), T::Error> {
    let mut chunk_buffer = [0; 3 * 128]; // TODO: find proper size
    let mut chunk_buffer_count = 0;
    // the packet that we're currently receiving
    let mut rx_packet: Option<RxPacket<M, N_BUF>> = None;
    // TODO: send ack also when packet was already fully received and processed
    let mut last_successfully_received_packet: Option<u16> = None;
    'outer: loop {
        // wait for at most PACKET_LEN_MAX, but the implementation should return after line idle detection anyways
        chunk_buffer_count += transport
            .read(&mut chunk_buffer[chunk_buffer_count..][..CHUNK_LEN_MAX])
            .await?;

        let mut buffer_start = 0;
        loop {
            // find the start of the next chunk
            let data_valid = &chunk_buffer[buffer_start..chunk_buffer_count];
            let Some(index_start) = data_valid.iter().position(|&v| v == MAGIC_0) else {
                // no chunk start found -> discard all data
                chunk_buffer_count = 0;
                continue 'outer;
            };

            // discard all data before the MAGIC marker
            buffer_start += index_start;
            let data_valid = &chunk_buffer[buffer_start..chunk_buffer_count];
            debug_assert!(data_valid[0] == MAGIC_0);

            // check that the minimum chunk length was received
            // MAGIC (2) + length (1) + type (1) + crc (4)
            if data_valid.len() < (2 + 1 + 1 + 4) {
                // chunk incomplete
                break;
            }

            // check second MAGIC byte
            if data_valid[1] != MAGIC_1 {
                // this is not a chunk start, continue search
                buffer_start += 1;
                continue;
            }

            // check chunk type
            let chunk_type = data_valid[3];
            match chunk_type {
                // valid chunk
                PACKET_DATA | PACKET_ACK => (),
                _ => {
                    // this is not a valid chunk, continue search
                    buffer_start += 1;
                    continue;
                }
            }

            // get chunk length
            let chunk_length = data_valid[2] as usize;
            if chunk_length > CHUNK_LEN_MAX {
                // this is not a valid chunk, continue search
                buffer_start += 1;
                continue;
            }

            // check if full chunk was received
            if data_valid.len() < (chunk_length + 4) {
                // chunk incomplete
                break;
            }

            // check crc
            let crc_calc = crc_dev.crc(&data_valid[..chunk_length]).await;
            let crc_read = u32::from_le_bytes(data_valid[chunk_length..][..4].try_into().unwrap());
            if crc_calc != crc_read {
                // this is not a valid chunk, continue search
                buffer_start += 1;
                defmt::warn!("received chunk with invalid crc");
                continue;
            }

            // `buffer_chunk` is `data_valid[4..chunk_length]`; need `chunk_length >= 4` for a valid range.
            if chunk_length < 4 {
                defmt::warn!("chunk length too short for header fields");
                buffer_start += chunk_length + 4;
                continue;
            }

            let buffer_chunk = &data_valid[4..chunk_length];
            // One frame is `chunk_length` bytes (CRC input) plus 4-byte CRC trailer.
            buffer_start += chunk_length + 4;

            match chunk_type {
                PACKET_DATA => {
                    // packet_id(2) + packet_len(4) + chunk_offset(4) + payload
                    const DATA_HEADER_BODY_LEN: usize = 2 + 4 + 4;
                    if buffer_chunk.len() < DATA_HEADER_BODY_LEN {
                        defmt::warn!("DATA chunk length too short for fixed header");
                        continue;
                    }

                    let packet_id = u16::from_le_bytes(buffer_chunk[0..2].try_into().unwrap());
                    let packet_length =
                        u32::from_le_bytes(buffer_chunk[2..6].try_into().unwrap()) as usize;
                    let chunk_offset =
                        u32::from_le_bytes(buffer_chunk[6..10].try_into().unwrap()) as usize;

                    let payload = &buffer_chunk[10..];

                    if packet_length > N_BUF {
                        defmt::warn!("received a chunk belonging to a packet that exceeds N_BUF");
                        continue;
                    }

                    if (chunk_offset * CHUNK_PAYLOAD_MAX + payload.len()) > packet_length {
                        defmt::warn!("received a chunk that exceeds its packet's length");
                        continue;
                    }

                    let payload_length_expected =
                        (packet_length - chunk_offset * CHUNK_PAYLOAD_MAX).min(CHUNK_PAYLOAD_MAX);
                    if payload.len() != payload_length_expected {
                        defmt::warn!(
                            "chunk payload length ({}) does not match chunk offset {} and packet length {}",
                            payload.len(),
                            chunk_offset,
                            packet_length
                        );
                        continue;
                    }

                    // check if this packet was received previously
                    if let Some(packet_id_last) = last_successfully_received_packet
                        && packet_id_last == packet_id
                    {
                        // send ack again
                        acks_to_send
                            .try_send(Ack {
                                packet_id,
                                chunk_offset: chunk_offset as u32,
                            })
                            .ok();
                        continue;
                    }

                    // in case we're already receiving a packet, check the packet id
                    if let Some(rxp) = rx_packet.as_ref()
                        && rxp.packet_id != packet_id
                    {
                        rx_packet = None;
                    }

                    // currently not receiving a packet --> allocate a new one
                    if rx_packet.is_none() {
                        // using "map" in case we can't allocate a buffer
                        rx_packet = buffer_pool.try_take().map(|buf| RxPacket {
                            buffer: buf,
                            packet_id,
                            packet_length,
                            buffer_written: [0; _],
                            buffer_written_count: 0,
                        })
                    }

                    // are we ready to receive?
                    let Some(rxp) = rx_packet.as_mut() else {
                        defmt::warn!(
                            "could not allocate a buffer for new packet with id {} and length {}",
                            packet_id,
                            packet_length,
                        );
                        continue;
                    };

                    // send ack
                    acks_to_send
                        .try_send(Ack {
                            packet_id,
                            chunk_offset: chunk_offset as u32,
                        })
                        .ok();

                    // copy to buffer
                    let rx_packet_buffer = &mut *rxp.buffer;
                    rx_packet_buffer[chunk_offset * CHUNK_PAYLOAD_MAX..][..payload.len()]
                        .copy_from_slice(payload);

                    // update buffer status
                    let id_num = chunk_offset / 32;
                    let id_bit = chunk_offset % 32;
                    let buffer_written = &mut rxp.buffer_written[id_num];
                    if (*buffer_written & (1 << id_bit)) == 0 {
                        rxp.buffer_written_count += 1;
                    }
                    *buffer_written |= 1 << id_bit;

                    // check packet received completely
                    let num_chunks = rxp.packet_length.div_ceil(CHUNK_PAYLOAD_MAX);
                    if rxp.buffer_written_count == num_chunks {
                        let length = rxp.packet_length;
                        let rx_packet = rx_packet.take().unwrap();
                        rx_queue
                            .send(BufferGuard::map(rx_packet.buffer, |buf| &mut buf[..length]))
                            .await;
                        last_successfully_received_packet = Some(rx_packet.packet_id);
                    }
                }
                PACKET_ACK => {
                    let mut buf = buffer_chunk;
                    while buf.len() >= 6 {
                        let ack = Ack::from_buffer(buf[..6].try_into().unwrap());
                        acks_received.try_send(ack).ok();
                        buf = &buf[6..];
                    }
                }
                t => {
                    defmt::warn!("received unknown packet type {:#02X}", t);
                    continue;
                }
            }
        }

        // move data to start
        if buffer_start == chunk_buffer_count {
            // no data remaining
            chunk_buffer_count = 0;
        } else if buffer_start != 0 {
            chunk_buffer.copy_within(buffer_start..chunk_buffer_count, 0);
            chunk_buffer_count -= buffer_start;
        }
    }
}

/// Sends `DATA` chunks from `tx_queue` and `ACK` frames from `ack_queue` to `transport`, appending
/// CRC after each frame body.
pub async fn run_write<M: RawMutex + 'static, T: TransportWrite, const N_INFLIGHT: usize>(
    transport: &mut T,
    tx_queue: &mut channel::Receiver<'_, M, BufferGuard<M, Vec<u8, CHUNK_LEN_MAX>>, N_INFLIGHT>,
    ack_queue: &mut channel::Receiver<'_, M, Ack, N_INFLIGHT>,
    crc_dev: &mut impl CrcDevice,
) -> Result<(), T::Error> {
    let mut ack_buf = Vec::<u8, CHUNK_LEN_MAX>::new();
    loop {
        match select(ack_queue.receive(), tx_queue.receive()).await {
            Either::First(ack) => {
                ack_buf.clear();
                // header
                ack_buf.extend_from_slice(&MAGIC).ok();
                // length
                let idx_len = ack_buf.len();
                ack_buf.push(0).ok();
                // type
                ack_buf.push(PACKET_ACK).ok();
                // acks
                ack_buf.extend_from_slice(&ack.to_buffer()).ok();
                // remaining acks (we need space for one ack and 4 bytes of crc)
                while (ack_buf.capacity() - ack_buf.len()) >= (6 + 4)
                    && let Ok(ack) = ack_queue.try_receive()
                {
                    ack_buf.extend_from_slice(&ack.to_buffer()).ok();
                }
                ack_buf[idx_len] = ack_buf.len() as u8;

                let crc = crc_dev.crc(&ack_buf).await;
                ack_buf.extend_from_slice(&crc.to_le_bytes()).ok();
                transport.write(&ack_buf).await?;
            }
            Either::Second(mut tx_buffer) => {
                let crc = crc_dev.crc(&tx_buffer).await;
                tx_buffer.extend_from_slice(&crc.to_le_bytes()).ok();
                transport.write(&tx_buffer).await?;
            }
        }
    }
}

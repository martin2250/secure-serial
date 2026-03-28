// #![no_std]
#![allow(async_fn_in_trait, dead_code)]
use embassy_futures::select::{Either, select};
use embassy_sync::{blocking_mutex::raw::RawMutex, channel, zerocopy_channel};
use embassy_time::{Duration, Instant, Timer};
use embedded_resource_pool::{MappedResourceGuard, ResourceGuard, ResourcePool};
use heapless::Vec;

pub trait TransportRead {
    type Error;
    async fn read(&mut self, data: &mut [u8]) -> Result<usize, Self::Error>;
}
pub trait TransportWrite {
    type Error;
    async fn write(&mut self, data: &[u8]) -> Result<(), Self::Error>;
}

pub trait CrcDevice {
    async fn crc(&mut self, data: &[u8]) -> u32;
}

/// how many packets can be in flight currently
const NUM_INFLIGHT: usize = 8;
const CHUNK_LEN_MAX: usize = 128;

/// used as a packet start identifier (SEcure SErial)
const MAGIC_0: u8 = 0x5E;
const MAGIC_1: u8 = 0x5E;
const MAGIC: [u8; 2] = [0x5E, 0x5E];
const PACKET_DATA: u8 = 0xDA;
const PACKET_ACK: u8 = 0xAC;
const PACKET_LEN_MAX: usize = CHUNK_LEN_MAX + 2 + 1 + 12 + 4; // MAGIC, TYPE, PACKET INFO, CRC

#[derive(Debug)]
pub struct Ack {
    packet_id: u16,
    chunk_offset: u32,
}

impl Ack {
    pub fn to_buffer(&self) -> [u8; 6] {
        let mut buffer = [0; 6];
        buffer[0..2].copy_from_slice(&self.packet_id.to_le_bytes());
        buffer[2..6].copy_from_slice(&self.chunk_offset.to_le_bytes());
        buffer
    }

    pub fn from_buffer(buffer: [u8; 6]) -> Self {
        let packet_id = u16::from_le_bytes(buffer[0..2].try_into().unwrap());
        let chunk_offset = u32::from_le_bytes(buffer[2..6].try_into().unwrap());

        Self {
            packet_id,
            chunk_offset,
        }
    }
}

pub struct SecureSerialSender<'a, M: RawMutex, C: CrcDevice> {
    write_packet_id: u16,
    allowed_retransmits: usize,

    tx_queue: zerocopy_channel::Sender<'a, M, Vec<u8, 160>>,
    rx_confirm: channel::Receiver<'a, M, Ack, NUM_INFLIGHT>,
    retransmit_delay: Duration,
    crc_dev: C,
}

impl<'a, M, C: CrcDevice> SecureSerialSender<'a, M, C>
where
    M: RawMutex,
{
    pub fn new(
        tx_queue: zerocopy_channel::Sender<'a, M, Vec<u8, 160>>,
        rx_confirm: channel::Receiver<'a, M, Ack, NUM_INFLIGHT>,
        retransmit_delay: Duration,
        allowed_retransmits: usize,
        crc_dev: C,
    ) -> Self {
        Self {
            write_packet_id: 0,
            allowed_retransmits,
            tx_queue,
            rx_confirm,
            retransmit_delay,
            crc_dev,
        }
    }

    pub async fn write_packet(&mut self, data: &[u8]) -> Result<(), ()> {
        self.write_packet_id += 1;

        let chunks_total = (data.len() + CHUNK_LEN_MAX - 1) / CHUNK_LEN_MAX;
        let mut chunk_next_queue = 0;

        struct ChunkInfo {
            chunk_offset: u32,
            last_sent_at: Instant,
            allowed_retransmits: usize,
        }

        let mut chunks: Vec<ChunkInfo, NUM_INFLIGHT> = Vec::new();

        loop {
            // sort buffer by next-to-send, b.cmp(a)
            // the oldest element is placed at the end, this one gets ack'ed or retransmitted first
            chunks.sort_unstable_by(|a, b| b.last_sent_at.cmp(&a.last_sent_at));

            // fill up chunk buffer, these ones should get transmitted first -> place at the end
            while !chunks.is_full() && chunk_next_queue < chunks_total {
                let info = ChunkInfo {
                    chunk_offset: chunk_next_queue as u32,
                    last_sent_at: Instant::MIN,
                    allowed_retransmits: self.allowed_retransmits,
                };
                chunks.push(info).ok();
                chunk_next_queue += 1;
            }

            // which chunk will get transmitted next
            let Some(next_chunk) = chunks.last_mut() else {
                // no chunk remaining? --> finished transmitting the packet
                break Ok(());
            };

            // wait for next chunk transmission delay, allow possible acks to be processed
            let next_chunk_tx_time = next_chunk.last_sent_at + self.retransmit_delay;
            let fut_tx = wait_then_tx(next_chunk_tx_time, &mut self.tx_queue);
            let fut_ack = self.rx_confirm.receive();

            // wait for slot in tx buffer
            let tx_buffer = match select(fut_tx, fut_ack).await {
                // tx buffer available
                Either::First(tx_buffer) => tx_buffer,
                // ack received -> update chunks
                Either::Second(ack) => {
                    // remove the matching chunk from the deque
                    if ack.packet_id == self.write_packet_id {
                        for (i, info) in chunks.iter().enumerate() {
                            if info.chunk_offset == ack.chunk_offset {
                                chunks.remove(i);
                                break;
                            }
                        }
                    }
                    // sort chunks again before sending
                    continue;
                }
            };

            println!("chunk retransmits: {}", next_chunk.allowed_retransmits);
            if next_chunk.allowed_retransmits == 0 {
                break Err(());
            }

            // encode packet
            let offset_bytes = next_chunk.chunk_offset as usize * CHUNK_LEN_MAX;
            let data_chunk = &data[offset_bytes..];
            let data_chunk = &data_chunk[..data_chunk.len().min(CHUNK_LEN_MAX)];

            tx_buffer.clear();
            tx_buffer.extend_from_slice(&MAGIC).ok();
            tx_buffer
                .push(
                    (MAGIC.len() // magic
                    + 1 // len
                    + 1 // packet type
                    + 2 // packet_id
                    + 4 // packet_len
                    + 4 // chunk_offset
                    + data_chunk.len()) as u8,
                )
                .ok();
            tx_buffer.push(PACKET_DATA).ok();

            let packet_id_encoded = self.write_packet_id.to_le_bytes();
            tx_buffer.extend_from_slice(&packet_id_encoded).ok();

            let packet_length_encoded = (data.len() as u32).to_le_bytes();
            tx_buffer.extend_from_slice(&packet_length_encoded).ok();

            let offset_encoded = next_chunk.chunk_offset.to_le_bytes();
            tx_buffer.extend_from_slice(&offset_encoded).ok();

            tx_buffer.extend_from_slice(data_chunk).ok();
            // crc is appended later

            next_chunk.last_sent_at = Instant::now();
            next_chunk.allowed_retransmits -= 1;

            self.tx_queue.send_done();
        }
    }
}

async fn wait_then_tx<'a, 'b, M: RawMutex>(
    at: Instant,
    tx_queue: &'b mut zerocopy_channel::Sender<'a, M, Vec<u8, 160>>,
) -> &'b mut Vec<u8, 160> {
    Timer::at(at).await;
    tx_queue.send().await
}

/// N_BUF: buffer size in multiples of 4k
pub struct SecureSerialReceiver<'a, M: RawMutex> {
    tx_queue: zerocopy_channel::Receiver<'a, M, Vec<u8, 160>>,
    ack_queue: channel::Channel<M, Ack, NUM_INFLIGHT>,
}

struct RxPacket<'a, M: RawMutex, const N_BUF: usize, const N_POOL: usize> {
    buffer: ResourceGuard<'a, 'static, M, [u8; N_BUF], N_POOL>,
    packet_id: u16,
    packet_length: usize,
    buffer_written: [u32; 4], // support at most 128 chunks / 16k bytes
    buffer_written_count: usize,
}

impl<'a, M: RawMutex> SecureSerialReceiver<'a, M> {
    pub async fn run(&mut self) {}

    pub async fn run_read<T: TransportRead, const N_POOL: usize, const N_BUF: usize>(
        transport: &mut T,
        crc_dev: &mut impl CrcDevice,
        buffer_pool: &'a ResourcePool<'static, M, [u8; N_BUF], N_POOL>,
        rx_queue: channel::Sender<
            '_,
            M,
            MappedResourceGuard<'a, 'static, M, [u8; N_BUF], [u8], N_POOL>,
            N_POOL,
        >,
        acks_to_send: channel::Sender<'_, M, Ack, NUM_INFLIGHT>,
        acks_received: channel::Sender<'_, M, Ack, NUM_INFLIGHT>,
    ) -> Result<(), T::Error> {
        let mut chunk_buffer = [0; 3 * 128]; // TODO: find proper size
        let mut chunk_buffer_count = 0;
        // the packet that we're currently receiving
        let mut rx_packet: Option<RxPacket<'_, M, N_BUF, N_POOL>> = None;
        // TODO: send ack also when packet was already fully received and processed
        let mut last_successfully_received_packet: Option<u16> = None;
        'outer: loop {
            // wait for at most PACKET_LEN_MAX, but the implementation should return after line idle detection anyways
            chunk_buffer_count += transport
                .read(&mut chunk_buffer[chunk_buffer_count..][..PACKET_LEN_MAX])
                .await?;

            let mut buffer_start = 0;
            loop {
                // find the start of the next chunk
                let data_valid = &chunk_buffer[buffer_start..chunk_buffer_count];
                let Some(index_start) = data_valid.iter().position(|&v| v == MAGIC_0) else {
                    // no chunk start found -> discard all data
                    println!("no chunk start found -> discard all data {data_valid:02x?}");
                    chunk_buffer_count = 0;
                    continue 'outer;
                };

                // discard all data before the MAGIC marker
                buffer_start += index_start;
                println!("index start: {index_start} buffer_start: {buffer_start}");
                let data_valid = &chunk_buffer[buffer_start..chunk_buffer_count];
                debug_assert!(data_valid[0] == MAGIC_0);

                // check that the minimum chunk length was received
                // MAGIC (2) + length (1) + type (1) + crc (4)
                if data_valid.len() < (2 + 1 + 1 + 4) {
                    // chunk incomplete
                    println!("chunk incomplete");
                    break;
                }

                // check second MAGIC byte
                if data_valid[1] != MAGIC_1 {
                    // this is not a chunk start, continue search
                    buffer_start += 1;
                    println!("this is not a chunk start, continue search {data_valid:02x?}");
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
                        println!("this is not a valid chunk, continue search chunk_type");
                        continue;
                    }
                }

                // get chunk length
                let chunk_length = data_valid[2] as usize;
                if chunk_length > PACKET_LEN_MAX {
                    // this is not a valid chunk, continue search
                    buffer_start += 1;
                    println!("not a valid chunk, continue search chunk_length > PACKET_LEN_MAX");
                    continue;
                }

                // check if full chunk was received
                if data_valid.len() < (chunk_length + 4) {
                    // chunk incomplete
                    println!("chunk incomplete data_valid.len() < (chunk_length + 4)");
                    break;
                }

                // check crc
                let crc_calc = crc_dev.crc(&data_valid[..chunk_length]).await;
                let crc_read =
                    u32::from_le_bytes(data_valid[chunk_length..][..4].try_into().unwrap());
                if crc_calc != crc_read {
                    // this is not a valid chunk, continue search
                    buffer_start += 1;
                    defmt::warn!("received chunk with invalid crc");
                    println!("received chunk with invalid crc {crc_calc:#08x} {crc_read:#08x}");
                    continue;
                }

                let buffer_chunk = &data_valid[4..chunk_length];
                // One frame is `chunk_length` bytes (CRC input) plus 4-byte CRC trailer.
                buffer_start += chunk_length + 4;

                match chunk_type {
                    PACKET_DATA => {
                        let packet_id = u16::from_le_bytes(buffer_chunk[0..2].try_into().unwrap());
                        let packet_length =
                            u32::from_le_bytes(buffer_chunk[2..6].try_into().unwrap()) as usize;
                        let chunk_offset =
                            u32::from_le_bytes(buffer_chunk[6..10].try_into().unwrap()) as usize;

                        let payload = &buffer_chunk[10..];

                        if packet_length > N_BUF {
                            defmt::warn!(
                                "received a chunk belonging to a packet that exceeds N_BUF"
                            );
                            println!("received a chunk belonging to a packet that exceeds N_BUF");
                            continue;
                        }

                        if (chunk_offset * CHUNK_LEN_MAX + payload.len()) > packet_length {
                            defmt::warn!("received a chunk that exceeds its packet's length");
                            println!(
                                "received a chunk that exceeds its packet's length chunk_offset: {} payload.len(): {} packet_length: {}",
                                chunk_offset,
                                payload.len(),
                                packet_length
                            );
                            continue;
                        }

                        let payload_length_expected =
                            (packet_length - chunk_offset * CHUNK_LEN_MAX).min(CHUNK_LEN_MAX);
                        if payload.len() != payload_length_expected {
                            defmt::warn!(
                                "chunk payload length ({}) does not match chunk offset {} and packet length {}",
                                payload.len(),
                                chunk_offset,
                                packet_length
                            );
                            println!(
                                "chunk payload length ({}) does not match chunk offset {} and packet length {}",
                                payload.len(),
                                chunk_offset,
                                packet_length
                            );
                            continue;
                        }

                        // check if this packet was received previously
                        if let Some(packet_id_last) = last_successfully_received_packet {
                            if packet_id_last == packet_id {
                                // send ack again
                                acks_to_send
                                    .try_send(Ack {
                                        packet_id,
                                        chunk_offset: chunk_offset as u32,
                                    })
                                    .ok();
                                continue;
                            }
                        }

                        // in case we're already receiving a packet, check the packet id
                        if let Some(rxp) = rx_packet.as_ref() {
                            if rxp.packet_id != packet_id {
                                rx_packet = None;
                            }
                        }

                        // currently not receiving a packet --> allocate a new one
                        if rx_packet.is_none() {
                            // using "map" in case we can't allocate a buffer
                            rx_packet = buffer_pool.try_take().map(|buf| RxPacket {
                                buffer: buf,
                                packet_id: packet_id,
                                packet_length: packet_length,
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
                        rx_packet_buffer[chunk_offset * CHUNK_LEN_MAX..][..payload.len()]
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
                        let num_chunks = (rxp.packet_length + (CHUNK_LEN_MAX - 1)) / CHUNK_LEN_MAX;
                        if rxp.buffer_written_count == num_chunks {
                            let length = rxp.packet_length;
                            let rx_packet = rx_packet.take().unwrap();
                            rx_queue
                                .send(ResourceGuard::map(rx_packet.buffer, |buf| {
                                    &mut buf[..length]
                                }))
                                .await;
                            last_successfully_received_packet = Some(rx_packet.packet_id);
                        }
                    }
                    PACKET_ACK => {
                        let mut buf = buffer_chunk;
                        while buf.len() >= 6 {
                            let ack = Ack::from_buffer(buf[..6].try_into().unwrap());
                            println!("received ack {:?}", ack);
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

    pub async fn run_write<T: TransportWrite>(
        transport: &mut T,
        tx_queue: &mut zerocopy_channel::Receiver<'a, M, Vec<u8, 160>>,
        ack_queue: &mut channel::Receiver<'_, M, Ack, NUM_INFLIGHT>,
        crc_dev: &mut impl CrcDevice,
    ) -> Result<(), T::Error> {
        let mut ack_buf = Vec::<u8, 160>::new();
        loop {
            // receive ack_queue in "a", so it has priority
            let mut rx_done = false;
            let tx_buffer = match select(ack_queue.receive(), tx_queue.receive()).await {
                Either::First(ack) => {
                    println!("run_write sending acks");
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
                    &mut ack_buf
                }
                Either::Second(tx_buffer) => {
                    println!("run_write sending data");
                    rx_done = true;
                    tx_buffer
                }
            };

            // calculate CRC
            let crc = crc_dev.crc(tx_buffer).await;
            tx_buffer.extend_from_slice(&crc.to_le_bytes()).ok();

            // send
            transport.write(&tx_buffer).await?;
            if rx_done {
                tx_queue.receive_done();
            }
        }
    }
}

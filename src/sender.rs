//! Application-facing [`SecureSerialSender`]: splits packets into chunks and cooperates with
//! [`crate::run_write`] and [`crate::run_read`] via shared pools and channels.

use embassy_futures::select::{Either, select};
use embassy_sync::{blocking_mutex::raw::RawMutex, channel};
use embassy_time::{Duration, Instant, Timer};
use embedded_buffer_pool::{BufferGuard, BufferPool};
use heapless::Vec;

use crate::protocol::{Ack, CHUNK_LEN_MAX, CHUNK_PAYLOAD_MAX, MAGIC, PACKET_DATA};

/// Sends logical packets over the link by chunking, queueing wire buffers, and waiting for ACKs.
pub struct SecureSerialSender<'a, M: RawMutex + 'static, const N_INFLIGHT: usize> {
    write_packet_id: u16,
    allowed_retransmits: usize,

    tx_pool: &'static BufferPool<M, Vec<u8, CHUNK_LEN_MAX>, N_INFLIGHT>,
    tx_queue: channel::Sender<'a, M, BufferGuard<M, Vec<u8, CHUNK_LEN_MAX>>, N_INFLIGHT>,
    rx_confirm: channel::Receiver<'a, M, Ack, N_INFLIGHT>,
    retransmit_delay: Duration,
}

impl<'a, M, const N_INFLIGHT: usize> SecureSerialSender<'a, M, N_INFLIGHT>
where
    M: RawMutex + 'static,
{
    /// Creates a sender using the shared TX pool and channels from [`crate::SecureSerialResources`].
    ///
    /// `retransmit_delay` spaces chunk transmissions; `allowed_retransmits` limits how often each
    /// chunk may be sent before [`write_packet`](Self::write_packet) fails.
    pub fn new(
        tx_pool: &'static BufferPool<M, Vec<u8, CHUNK_LEN_MAX>, N_INFLIGHT>,
        tx_queue: channel::Sender<'a, M, BufferGuard<M, Vec<u8, CHUNK_LEN_MAX>>, N_INFLIGHT>,
        rx_confirm: channel::Receiver<'a, M, Ack, N_INFLIGHT>,
        retransmit_delay: Duration,
        allowed_retransmits: usize,
    ) -> Self {
        Self {
            write_packet_id: 0,
            allowed_retransmits,
            tx_pool,
            tx_queue,
            rx_confirm,
            retransmit_delay,
        }
    }

    /// Encodes `data` as one packet id, splits it into chunks, and returns when all chunks are ACKed
    /// or retries are exhausted (`Err(())`).
    pub async fn write_packet(&mut self, data: &[u8]) -> Result<(), ()> {
        self.write_packet_id += 1;

        let chunks_total = data.len().div_ceil(CHUNK_PAYLOAD_MAX);
        let mut chunk_next_queue = 0;

        struct ChunkInfo {
            chunk_offset: u32,
            last_sent_at: Instant,
            allowed_retransmits: usize,
        }

        let mut chunks: Vec<ChunkInfo, N_INFLIGHT> = Vec::new();

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
            let fut_tx = wait_theN_INFLIGHT(next_chunk_tx_time, self.tx_pool);
            let fut_ack = self.rx_confirm.receive();

            // wait for slot in tx buffer
            let mut tx_buffer = match select(fut_tx, fut_ack).await {
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

            if next_chunk.allowed_retransmits == 0 {
                break Err(());
            }

            // encode packet
            let offset_bytes = next_chunk.chunk_offset as usize * CHUNK_PAYLOAD_MAX;
            let data_chunk = &data[offset_bytes..];
            let data_chunk = &data_chunk[..data_chunk.len().min(CHUNK_PAYLOAD_MAX)];

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

            self.tx_queue.send(tx_buffer).await;
        }
    }
}

async fn wait_theN_INFLIGHT<M: RawMutex + 'static, const N: usize>(
    at: Instant,
    tx_pool: &'static BufferPool<M, Vec<u8, CHUNK_LEN_MAX>, N>,
) -> BufferGuard<M, Vec<u8, CHUNK_LEN_MAX>> {
    Timer::at(at).await;
    tx_pool.take().await
}

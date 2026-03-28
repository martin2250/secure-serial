use embassy_futures::select::{Either, select};
use embassy_sync::{blocking_mutex::raw::RawMutex, channel, zerocopy_channel};
use embassy_time::{Duration, Instant, Timer};
use heapless::Vec;

use crate::protocol::{
    Ack, CHUNK_LEN_MAX, MAGIC, NUM_INFLIGHT, PACKET_DATA,
};

pub struct SecureSerialSender<'a, M: RawMutex> {
    write_packet_id: u16,
    allowed_retransmits: usize,

    tx_queue: zerocopy_channel::Sender<'a, M, Vec<u8, 160>>,
    rx_confirm: channel::Receiver<'a, M, Ack, NUM_INFLIGHT>,
    retransmit_delay: Duration,
}

impl<'a, M> SecureSerialSender<'a, M>
where
    M: RawMutex,
{
    pub fn new(
        tx_queue: zerocopy_channel::Sender<'a, M, Vec<u8, 160>>,
        rx_confirm: channel::Receiver<'a, M, Ack, NUM_INFLIGHT>,
        retransmit_delay: Duration,
        allowed_retransmits: usize,
    ) -> Self {
        Self {
            write_packet_id: 0,
            allowed_retransmits,
            tx_queue,
            rx_confirm,
            retransmit_delay,
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

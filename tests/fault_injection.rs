//! Integration tests: secure-serial over scripted bidirectional links with fault injection.
//! Category: Integration (host-side fake transport, no mock frameworks).

use crc::{CRC_32_ISO_HDLC, Crc};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel};
use embassy_time::Duration;
use embedded_buffer_pool::{BufferGuard, BufferPool, MappedBufferGuard, array_new};
use heapless::Vec as HeaplessVec;
use secure_serial::{
    Ack, CHUNK_LEN_MAX, CrcDevice, SecureSerialSender, TransportRead, TransportWrite, run_read,
    run_write,
};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};

const FRAME_DATA: u8 = 0xDA;
const FRAME_ACK: u8 = 0xAC;

struct SoftwareCrc;

impl CrcDevice for SoftwareCrc {
    async fn crc(&mut self, data: &[u8]) -> u32 {
        const SW_CRC: Crc<u32> = Crc::<u32>::new(&CRC_32_ISO_HDLC);
        SW_CRC.checksum(data)
    }
}

#[derive(Clone, Default)]
enum WriteRule {
    #[default]
    Passthrough,
    DropFirstData {
        consumed: bool,
    },
    /// Drop the first N DATA frames on this link (for multi-loss scenarios).
    DropFirstNData {
        remaining: usize,
    },
    DropAllData,
    CorruptByteFirstData {
        consumed: bool,
        byte_index: usize,
        xor: u8,
    },
    CorruptLengthFirstData {
        consumed: bool,
    },
    CorruptCrcFirstData {
        consumed: bool,
    },
    SplitFirstData {
        consumed: bool,
        at: usize,
    },
    DuplicateFirstData {
        consumed: bool,
    },
    PrependGarbageFirstData {
        consumed: bool,
        prefix: std::vec::Vec<u8>,
    },
    PrependFalseMagicFirstData {
        consumed: bool,
    },
    DropFirstAck {
        consumed: bool,
    },
}

fn apply_write_rule(rule: &mut WriteRule, data: &[u8]) -> std::vec::Vec<std::vec::Vec<u8>> {
    let is_data = data.get(3) == Some(&FRAME_DATA);
    let is_ack = data.get(3) == Some(&FRAME_ACK);

    match rule {
        WriteRule::Passthrough => vec![data.to_vec()],
        WriteRule::DropFirstData { consumed } => {
            if is_data && !*consumed {
                *consumed = true;
                vec![]
            } else {
                vec![data.to_vec()]
            }
        }
        WriteRule::DropFirstNData { remaining } => {
            if is_data && *remaining > 0 {
                *remaining -= 1;
                vec![]
            } else {
                vec![data.to_vec()]
            }
        }
        WriteRule::DropAllData => {
            if is_data {
                vec![]
            } else {
                vec![data.to_vec()]
            }
        }
        WriteRule::CorruptByteFirstData {
            consumed,
            byte_index,
            xor,
        } => {
            if is_data && !*consumed {
                *consumed = true;
                let mut v = data.to_vec();
                if let Some(b) = v.get_mut(*byte_index) {
                    *b ^= *xor;
                }
                vec![v]
            } else {
                vec![data.to_vec()]
            }
        }
        WriteRule::CorruptLengthFirstData { consumed } => {
            if is_data && !*consumed {
                *consumed = true;
                let mut v = data.to_vec();
                if v.len() > 2 {
                    v[2] = 0xFF;
                }
                vec![v]
            } else {
                vec![data.to_vec()]
            }
        }
        WriteRule::CorruptCrcFirstData { consumed } => {
            if is_data && !*consumed {
                *consumed = true;
                let mut v = data.to_vec();
                if let Some(last) = v.last_mut() {
                    *last ^= 0x5A;
                }
                vec![v]
            } else {
                vec![data.to_vec()]
            }
        }
        WriteRule::SplitFirstData { consumed, at } => {
            if is_data && !*consumed {
                *consumed = true;
                let at = (*at).min(data.len());
                vec![data[..at].to_vec(), data[at..].to_vec()]
            } else {
                vec![data.to_vec()]
            }
        }
        WriteRule::DuplicateFirstData { consumed } => {
            if is_data && !*consumed {
                *consumed = true;
                let v = data.to_vec();
                vec![v.clone(), v]
            } else {
                vec![data.to_vec()]
            }
        }
        WriteRule::PrependGarbageFirstData { consumed, prefix } => {
            if is_data && !*consumed {
                *consumed = true;
                let mut out = prefix.clone();
                out.extend_from_slice(data);
                vec![out]
            } else {
                vec![data.to_vec()]
            }
        }
        WriteRule::PrependFalseMagicFirstData { consumed } => {
            if is_data && !*consumed {
                *consumed = true;
                let mut out = vec![0x5Eu8, 0x00];
                out.extend_from_slice(data);
                vec![out]
            } else {
                vec![data.to_vec()]
            }
        }
        WriteRule::DropFirstAck { consumed } => {
            if is_ack && !*consumed {
                *consumed = true;
                vec![]
            } else {
                vec![data.to_vec()]
            }
        }
    }
}

struct QueueReadTransport {
    queue: Arc<Mutex<VecDeque<u8>>>,
    notify: Arc<Notify>,
    max_per_read: Option<usize>,
}

impl TransportRead for QueueReadTransport {
    type Error = ();

    async fn read(&mut self, data: &mut [u8]) -> Result<usize, Self::Error> {
        if data.is_empty() {
            return Ok(0);
        }
        loop {
            let mut q = self.queue.lock().await;
            if !q.is_empty() {
                let avail = q.len();
                let cap = data.len();
                let chunk = match self.max_per_read {
                    None => avail.min(cap),
                    Some(m) => avail.min(cap).min(m.max(1)),
                };
                for i in 0..chunk {
                    data[i] = q.pop_front().unwrap();
                }
                return Ok(chunk);
            }
            let fut = self.notify.notified();
            drop(q);
            fut.await;
        }
    }
}

struct QueueWriteTransport {
    queue: Arc<Mutex<VecDeque<u8>>>,
    notify: Arc<Notify>,
    rule: Arc<Mutex<WriteRule>>,
}

impl QueueWriteTransport {
    fn new(queue: Arc<Mutex<VecDeque<u8>>>, notify: Arc<Notify>, rule: WriteRule) -> Self {
        Self {
            queue,
            notify,
            rule: Arc::new(Mutex::new(rule)),
        }
    }
}

impl TransportWrite for QueueWriteTransport {
    type Error = ();

    async fn write(&mut self, data: &[u8]) -> Result<(), Self::Error> {
        let segments = {
            let mut r = self.rule.lock().await;
            apply_write_rule(&mut *r, data)
        };
        let mut q = self.queue.lock().await;
        for seg in segments {
            q.extend(seg);
        }
        drop(q);
        self.notify.notify_waiters();
        Ok(())
    }
}

struct HarnessConfig {
    ab_rule: WriteRule,
    ba_rule: WriteRule,
    ab_read_max: Option<usize>,
    ba_read_max: Option<usize>,
    retransmit_delay: Duration,
    allowed_retransmits: usize,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            ab_rule: WriteRule::Passthrough,
            ba_rule: WriteRule::Passthrough,
            ab_read_max: None,
            ba_read_max: None,
            retransmit_delay: Duration::from_millis(5),
            allowed_retransmits: 8,
        }
    }
}

/// A sends one `payload` to B. Outer `Err` is test timeout; inner `Err` is sender failure.
async fn transfer_a_to_b(
    payload: &[u8],
    cfg: HarnessConfig,
) -> Result<std::result::Result<std::vec::Vec<u8>, ()>, tokio::time::error::Elapsed> {
    let ab_queue = Arc::new(Mutex::new(VecDeque::new()));
    let ba_queue = Arc::new(Mutex::new(VecDeque::new()));
    let ab_notify = Arc::new(Notify::new());
    let ba_notify = Arc::new(Notify::new());

    let a_read = QueueReadTransport {
        queue: ba_queue.clone(),
        notify: ba_notify.clone(),
        max_per_read: cfg.ba_read_max,
    };
    let a_write =
        QueueWriteTransport::new(ab_queue.clone(), ab_notify.clone(), cfg.ab_rule.clone());
    let b_read = QueueReadTransport {
        queue: ab_queue.clone(),
        notify: ab_notify.clone(),
        max_per_read: cfg.ab_read_max,
    };
    let b_write =
        QueueWriteTransport::new(ba_queue.clone(), ba_notify.clone(), cfg.ba_rule.clone());

    // --- Side A (sender) ---
    let a_tx_alloc = Box::leak(Box::new(BufferPool::<
        CriticalSectionRawMutex,
        HeaplessVec<u8, CHUNK_LEN_MAX>,
        8,
    >::new(array_new!(HeaplessVec::new(), 8))));
    let a_tx_ch = Box::leak(Box::new(channel::Channel::<
        CriticalSectionRawMutex,
        BufferGuard<CriticalSectionRawMutex, HeaplessVec<u8, CHUNK_LEN_MAX>>,
        8,
    >::new()));
    let a_tx_ch_tx = a_tx_ch.sender();
    let a_tx_ch_rx = a_tx_ch.receiver();

    let a_acks_to_send = Box::leak(Box::new(
        channel::Channel::<CriticalSectionRawMutex, Ack, 8>::new(),
    ));
    let a_acks_received = Box::leak(Box::new(
        channel::Channel::<CriticalSectionRawMutex, Ack, 8>::new(),
    ));

    let a_rx_alloc = Box::leak(Box::new(
        BufferPool::<CriticalSectionRawMutex, [u8; 4096], 4>::new([[0u8; 4096]; 4]),
    ));
    let a_rx_queue = Box::leak(Box::new(channel::Channel::<
        CriticalSectionRawMutex,
        MappedBufferGuard<CriticalSectionRawMutex, [u8]>,
        4,
    >::new()));

    tokio::task::spawn_local(run_write(
        Box::leak(Box::new(a_write)),
        Box::leak(Box::new(a_tx_ch_rx)),
        Box::leak(Box::new(a_acks_to_send.receiver())),
        Box::leak(Box::new(SoftwareCrc)),
    ));

    tokio::task::spawn_local(run_read(
        Box::leak(Box::new(a_read)),
        Box::leak(Box::new(SoftwareCrc)),
        a_rx_alloc,
        a_rx_queue.sender(),
        a_acks_to_send.sender(),
        a_acks_received.sender(),
    ));

    // --- Side B (receiver only; no data frames — tx channel stays idle) ---
    let b_tx_ch = Box::leak(Box::new(channel::Channel::<
        CriticalSectionRawMutex,
        BufferGuard<CriticalSectionRawMutex, HeaplessVec<u8, CHUNK_LEN_MAX>>,
        8,
    >::new()));
    let b_tx_ch_rx = b_tx_ch.receiver();

    let b_acks_to_send = Box::leak(Box::new(
        channel::Channel::<CriticalSectionRawMutex, Ack, 8>::new(),
    ));
    let b_acks_received = Box::leak(Box::new(
        channel::Channel::<CriticalSectionRawMutex, Ack, 8>::new(),
    ));

    let b_rx_alloc = Box::leak(Box::new(
        BufferPool::<CriticalSectionRawMutex, [u8; 4096], 4>::new([[0u8; 4096]; 4]),
    ));
    let b_rx_queue = Box::leak(Box::new(channel::Channel::<
        CriticalSectionRawMutex,
        MappedBufferGuard<CriticalSectionRawMutex, [u8]>,
        4,
    >::new()));
    let b_rx_from_queue = b_rx_queue.receiver();

    let (delivered_tx, mut delivered_rx) = tokio::sync::mpsc::channel::<std::vec::Vec<u8>>(2);

    tokio::task::spawn_local(run_write(
        Box::leak(Box::new(b_write)),
        Box::leak(Box::new(b_tx_ch_rx)),
        Box::leak(Box::new(b_acks_to_send.receiver())),
        Box::leak(Box::new(SoftwareCrc)),
    ));

    tokio::task::spawn_local(run_read(
        Box::leak(Box::new(b_read)),
        Box::leak(Box::new(SoftwareCrc)),
        b_rx_alloc,
        b_rx_queue.sender(),
        b_acks_to_send.sender(),
        b_acks_received.sender(),
    ));

    tokio::task::spawn_local(async move {
        let guard = b_rx_from_queue.receive().await;
        let _ = delivered_tx.send(guard.to_vec()).await;
    });

    let mut sender = SecureSerialSender::new(
        a_tx_alloc,
        a_tx_ch_tx,
        a_acks_received.receiver(),
        cfg.retransmit_delay,
        cfg.allowed_retransmits,
    );

    tokio::time::timeout(std::time::Duration::from_secs(5), async move {
        match sender.write_packet(payload).await {
            Ok(()) => {
                let v = delivered_rx
                    .recv()
                    .await
                    .expect("B should deliver one packet");
                Ok(v)
            }
            Err(()) => Err(()),
        }
    })
    .await
}

fn local_test(
    payload: &[u8],
    cfg: HarnessConfig,
) -> Result<std::result::Result<std::vec::Vec<u8>, ()>, tokio::time::error::Elapsed> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async {
            let local = tokio::task::LocalSet::new();
            local.run_until(transfer_a_to_b(payload, cfg)).await
        })
}

// --- A: lost / split reads ---

#[test]
fn a1_trailing_split_read_recovers_payload() {
    let p = b"test hello";
    let mut c = HarnessConfig::default();
    c.ab_read_max = Some(7);
    let got = local_test(p, c).expect("timeout");
    assert_eq!(got.expect("send ok"), p.to_vec());
}

#[test]
fn a2_split_on_single_byte_reads() {
    let p = b"xy";
    let mut c = HarnessConfig::default();
    c.ab_read_max = Some(1);
    let got = local_test(p, c).expect("timeout");
    assert_eq!(got.expect("send ok"), p.to_vec());
}

#[test]
fn a3_garbage_prefix_before_frame_recovers_payload() {
    let p = b"ping";
    let mut c = HarnessConfig::default();
    c.ab_rule = WriteRule::PrependGarbageFirstData {
        consumed: false,
        prefix: vec![0x00, 0xAB, 0xCD],
    };
    let got = local_test(p, c).expect("timeout");
    assert_eq!(got.expect("send ok"), p.to_vec());
}

#[test]
fn a4_false_magic_prefix_recovers_payload() {
    let p = b"z";
    let mut c = HarnessConfig::default();
    c.ab_rule = WriteRule::PrependFalseMagicFirstData { consumed: false };
    let got = local_test(p, c).expect("timeout");
    assert_eq!(got.expect("send ok"), p.to_vec());
}

// --- B: corruption ---

#[test]
fn b1_payload_corruption_then_clean_retransmit_delivers_once() {
    let p = b"payload";
    let mut c = HarnessConfig::default();
    // Flip a byte in the application payload (index chosen after stable header layout).
    c.ab_rule = WriteRule::CorruptByteFirstData {
        consumed: false,
        byte_index: 20,
        xor: 0xFF,
    };
    let got = local_test(p, c).expect("timeout");
    assert_eq!(got.expect("send ok"), p.to_vec());
}

#[test]
fn b2_corrupt_length_first_data_then_retry() {
    let p = b"ok";
    let mut c = HarnessConfig::default();
    c.ab_rule = WriteRule::CorruptLengthFirstData { consumed: false };
    let got = local_test(p, c).expect("timeout");
    assert_eq!(got.expect("send ok"), p.to_vec());
}

#[test]
fn b3_corrupt_crc_first_data_then_retry() {
    let p = b"crc";
    let mut c = HarnessConfig::default();
    c.ab_rule = WriteRule::CorruptCrcFirstData { consumed: false };
    let got = local_test(p, c).expect("timeout");
    assert_eq!(got.expect("send ok"), p.to_vec());
}

// --- C: loss / ACK loss ---

#[test]
fn c1_drop_first_data_frame_retransmit_delivers_once() {
    let p = b"once";
    let mut c = HarnessConfig::default();
    c.ab_rule = WriteRule::DropFirstData { consumed: false };
    let got = local_test(p, c).expect("timeout");
    assert_eq!(got.expect("send ok"), p.to_vec());
}

#[test]
fn c2_drop_first_ack_forces_data_retransmit_still_one_delivery() {
    let p = b"ackloss";
    let mut c = HarnessConfig::default();
    c.ba_rule = WriteRule::DropFirstAck { consumed: false };
    let got = local_test(p, c).expect("timeout");
    assert_eq!(got.expect("send ok"), p.to_vec());
}

#[test]
fn c3_drop_first_two_data_frames_then_succeeds() {
    let p = b"twice";
    let mut c = HarnessConfig::default();
    c.ab_rule = WriteRule::DropFirstNData { remaining: 2 };
    let got = local_test(p, c).expect("timeout");
    assert_eq!(got.expect("send ok"), p.to_vec());
}

#[test]
fn c4_exhausted_retransmits_returns_error() {
    let p = b"fail";
    let mut c = HarnessConfig::default();
    c.ab_rule = WriteRule::DropAllData;
    c.allowed_retransmits = 2;
    let got = local_test(p, c).expect("timeout");
    assert!(
        got.is_err(),
        "sender should fail when all DATA frames are dropped"
    );
}

// --- D: multi-chunk ---

#[test]
fn d1_two_chunk_packet_drop_first_data_recovers() {
    let p = [0xABu8; 200];
    let mut c = HarnessConfig::default();
    c.ab_rule = WriteRule::DropFirstData { consumed: false };
    let got = local_test(&p, c).expect("timeout");
    assert_eq!(got.expect("send ok"), p.to_vec());
}

// --- F: large payloads (> 1 KiB); harness RX buffers are 4 KiB ---

#[test]
fn f1_large_payload_1536_bytes_passthrough() {
    let p: std::vec::Vec<u8> = (0..1536).map(|i| i as u8).collect();
    let got = local_test(&p, HarnessConfig::default()).expect("timeout");
    assert_eq!(got.expect("send ok"), p);
}

#[test]
fn f2_large_payload_2048_bytes_split_reads() {
    let p: std::vec::Vec<u8> = (0..2048)
        .map(|i| ((i as u32).wrapping_mul(17).wrapping_add(3)) as u8)
        .collect();
    let mut c = HarnessConfig::default();
    c.ab_read_max = Some(29);
    let got = local_test(&p, c).expect("timeout");
    assert_eq!(got.expect("send ok"), p);
}

#[test]
fn f3_large_payload_4096_bytes_drop_first_chunk_recovers() {
    let p: std::vec::Vec<u8> = (0..4096).map(|i| (i ^ (i >> 7)) as u8).collect();
    let mut c = HarnessConfig::default();
    c.ab_rule = WriteRule::DropFirstData { consumed: false };
    let got = local_test(&p, c).expect("timeout");
    assert_eq!(got.expect("send ok"), p);
}

// --- E: duplicate on-wire DATA ---

#[test]
fn e1_duplicate_first_data_frame_delivers_payload_once() {
    let p = b"dup";
    let mut c = HarnessConfig::default();
    c.ab_rule = WriteRule::DuplicateFirstData { consumed: false };
    let got = local_test(p, c).expect("timeout");
    assert_eq!(got.expect("send ok"), p.to_vec());
}

#[test]
fn a_split_mid_frame_wire() {
    let p = b"splitme";
    let mut c = HarnessConfig::default();
    c.ab_rule = WriteRule::SplitFirstData {
        consumed: false,
        at: 11,
    };
    let got = local_test(p, c).expect("timeout");
    assert_eq!(got.expect("send ok"), p.to_vec());
}

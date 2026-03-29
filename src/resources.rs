//! Owned queues and buffer pools for one side of a secure-serial link.
//!
//! This module is described in detail on [`SecureSerialResources`]; see the crate README for how
//! tasks and [`crate::SecureSerialSender`] connect to the channels here.

use embassy_sync::{blocking_mutex::raw::RawMutex, channel};
use embedded_buffer_pool::{BufferGuard, BufferPool, MappedBufferGuard};
use heapless::Vec;

use crate::protocol::{Ack, CHUNK_LEN_MAX};

/// Queues and pools for [`crate::run_read`], [`crate::run_write`], and [`crate::SecureSerialSender`]
/// on a single endpoint (one UART side).
///
/// This type groups the TX chunk pool, TX/RX [`embassy_sync::channel::Channel`]s, and the RX
/// reassembly pool so you do not wire six separate static items by hand.
///
/// # Sizes
///
/// - `N_INFLIGHT`: capacity of the outgoing chunk queue, TX [`BufferPool`], and ACK channels (same as
///   in-flight chunk/ACK window); must be in 1..=32.
/// - `N_RX_POOL`: number of parallel RX packet buffers and RX completion queue depth (1..=32).
/// - `N_BUF`: byte length of each RX reassembly buffer (must fit your largest packet).
///
/// [`BufferPool`] and channels require fixed sizes in that range.
///
/// # `static` placement
///
/// [`embedded_buffer_pool::BufferPool`] only exposes `take` / `try_take` on `&'static` pools.
/// Put this struct in a `static_cell::StaticCell` (or another `'static` slot) so references
/// passed to [`crate::SecureSerialSender::new`] and async tasks are valid for the whole program.
///
/// # Example (Cortex-M / `no_std`)
///
/// Store the bundle once in a `static_cell::StaticCell`, then reborrow it as a shared
/// `&'static SecureSerialResources<...>` so every task can hold a copy of the pointer (the
/// mutex-backed channels inside are safe to share). Use [`SecureSerialResourcesDefault`] if the
/// default sizes (8 TX slots, 4 × 4096-byte RX buffers) fit your link.
///
/// ```ignore
/// use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
/// use embassy_executor::Spawner;
/// use static_cell::StaticCell;
/// use secure_serial::{SecureSerialResources, SecureSerialResourcesDefault};
///
/// static SERIAL_RES: StaticCell<SecureSerialResourcesDefault<CriticalSectionRawMutex>> =
///     StaticCell::new();
///
/// fn init_serial_tasks(spawner: Spawner) {
///     let res_mut = SERIAL_RES.init(SecureSerialResourcesDefault::new());
///     let res: &'static SecureSerialResourcesDefault<CriticalSectionRawMutex> = res_mut;
///
///     // `tx_pool()` and channel endpoints are valid for `'static` because `res` is.
///     let mut tx_rx = res.tx_chunks_receiver();
///     let mut acks_out_rx = res.acks_to_wire_receiver();
///     unwrap!(spawner.spawn(run_write_task(&mut tx_rx, &mut acks_out_rx)));
///     unwrap!(spawner.spawn(run_read_task(res)));
///     unwrap!(spawner.spawn(app_task(res)));
/// }
///
/// #[embassy_executor::task]
/// async fn run_write_task(
///     tx_rx: &mut /* `Receiver` for DATA chunks from `tx_chunks_receiver` */,
///     acks_out_rx: &mut /* `Receiver` for ACKs from `acks_to_wire_receiver` */,
/// ) {
///     let _ = secure_serial::run_write(&mut /* uart */, tx_rx, acks_out_rx, &mut /* crc */).await;
/// }
/// ```
pub struct SecureSerialResources<
    M: RawMutex + 'static,
    const N_INFLIGHT: usize,
    const N_RX_POOL: usize,
    const N_BUF: usize,
> {
    tx_pool: BufferPool<M, Vec<u8, CHUNK_LEN_MAX>, N_INFLIGHT>,
    tx_chunks: channel::Channel<M, BufferGuard<M, Vec<u8, CHUNK_LEN_MAX>>, N_INFLIGHT>,
    acks_to_send: channel::Channel<M, Ack, N_INFLIGHT>,
    acks_received: channel::Channel<M, Ack, N_INFLIGHT>,
    rx_pool: BufferPool<M, [u8; N_BUF], N_RX_POOL>,
    rx_complete: channel::Channel<M, MappedBufferGuard<M, [u8]>, N_RX_POOL>,
}

/// Common defaults: 8 TX slots, 4 RX buffers, 4096 bytes per RX buffer.
pub type SecureSerialResourcesDefault<M> = SecureSerialResources<M, 8, 4, 4096>;

impl<M: RawMutex + 'static, const N_INFLIGHT: usize, const N_RX_POOL: usize, const N_BUF: usize>
    SecureSerialResources<M, N_INFLIGHT, N_RX_POOL, N_BUF>
{
    /// Builds pools and channels; all buffers start empty and channels empty.
    ///
    /// # Panics
    ///
    /// If `N_INFLIGHT` or `N_RX_POOL` is not in `1..=32`.
    pub fn new() -> Self {
        assert!(N_INFLIGHT > 0 && N_INFLIGHT <= 32);
        assert!(N_RX_POOL > 0 && N_RX_POOL <= 32);

        let tx_backing = core::array::from_fn(|_| Vec::new());
        let tx_pool = BufferPool::new(tx_backing);
        let tx_chunks = channel::Channel::new();
        let acks_to_send = channel::Channel::new();
        let acks_received = channel::Channel::new();
        let rx_backing = core::array::from_fn(|_| [0u8; N_BUF]);
        let rx_pool = BufferPool::new(rx_backing);
        let rx_complete = channel::Channel::new();
        Self {
            tx_pool,
            tx_chunks,
            acks_to_send,
            acks_received,
            rx_pool,
            rx_complete,
        }
    }

    /// Pool for outbound DATA chunks; pass to [`crate::SecureSerialSender::new`] when `self` is `'static`.
    #[inline]
    pub fn tx_pool(&self) -> &BufferPool<M, Vec<u8, CHUNK_LEN_MAX>, N_INFLIGHT> {
        &self.tx_pool
    }

    /// Sender for encoded DATA chunks consumed by [`crate::run_write`].
    #[inline]
    pub fn tx_chunks_sender(
        &self,
    ) -> channel::Sender<'_, M, BufferGuard<M, Vec<u8, CHUNK_LEN_MAX>>, N_INFLIGHT> {
        self.tx_chunks.sender()
    }

    /// Receiver used by [`crate::run_write`] for pending DATA frames.
    #[inline]
    pub fn tx_chunks_receiver(
        &self,
    ) -> channel::Receiver<'_, M, BufferGuard<M, Vec<u8, CHUNK_LEN_MAX>>, N_INFLIGHT> {
        self.tx_chunks.receiver()
    }

    /// ACKs generated locally that must be written to the wire (feed [`crate::run_write`]).
    #[inline]
    pub fn acks_to_wire_sender(&self) -> channel::Sender<'_, M, Ack, N_INFLIGHT> {
        self.acks_to_send.sender()
    }

    #[inline]
    pub fn acks_to_wire_receiver(&self) -> channel::Receiver<'_, M, Ack, N_INFLIGHT> {
        self.acks_to_send.receiver()
    }

    /// ACKs received from the peer (feed [`crate::SecureSerialSender::new`] as `rx_confirm`).
    #[inline]
    pub fn acks_from_peer_sender(&self) -> channel::Sender<'_, M, Ack, N_INFLIGHT> {
        self.acks_received.sender()
    }

    #[inline]
    pub fn acks_from_peer_receiver(&self) -> channel::Receiver<'_, M, Ack, N_INFLIGHT> {
        self.acks_received.receiver()
    }

    /// Pool for partially reassembled RX packets; pass to [`crate::run_read`] when `self` is `'static`.
    #[inline]
    pub fn rx_pool(&self) -> &BufferPool<M, [u8; N_BUF], N_RX_POOL> {
        &self.rx_pool
    }

    /// Completed packets from [`crate::run_read`].
    #[inline]
    pub fn rx_complete_sender(
        &self,
    ) -> channel::Sender<'_, M, MappedBufferGuard<M, [u8]>, N_RX_POOL> {
        self.rx_complete.sender()
    }

    #[inline]
    pub fn rx_complete_receiver(
        &self,
    ) -> channel::Receiver<'_, M, MappedBufferGuard<M, [u8]>, N_RX_POOL> {
        self.rx_complete.receiver()
    }
}

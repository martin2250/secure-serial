use crc::{CRC_32_ISO_HDLC, Crc};
use embassy_sync::{
    blocking_mutex::raw::{RawMutex, ThreadModeRawMutex},
    channel,
    pipe::{Pipe, Reader, Writer},
};
use embassy_time::{Duration, Timer};
use embedded_buffer_pool::{BufferGuard, BufferPool, MappedBufferGuard, array_new};
use heapless::Vec;
use secure_serial::{
    Ack, CHUNK_LEN_MAX, CrcDevice, SecureSerialSender, TransportRead, TransportWrite, run_read,
    run_write,
};
use static_cell::ConstStaticCell;

static PIPE_A_TO_B: ConstStaticCell<Pipe<ThreadModeRawMutex, 200>> =
    ConstStaticCell::new(Pipe::new());
static PIPE_B_TO_A: ConstStaticCell<Pipe<ThreadModeRawMutex, 200>> =
    ConstStaticCell::new(Pipe::new());

#[tokio::main(flavor = "local")]

async fn main() {
    let (a_to_b_rx, a_to_b_tx) = PIPE_A_TO_B.take().split();
    let (b_to_a_rx, b_to_a_tx) = PIPE_B_TO_A.take().split();

    let a_read = PipeTransportRead { inner: b_to_a_rx };
    let a_write = PipeTransportWrite { inner: a_to_b_tx };
    let b_read = PipeTransportRead { inner: a_to_b_rx };
    let b_write = PipeTransportWrite { inner: b_to_a_tx };

    tokio::task::spawn_local(run_one_side(a_read, a_write, true));
    tokio::task::spawn_local(run_one_side(b_read, b_write, false));

    loop {
        Timer::after_secs(1).await;
    }
}

async fn run_one_side<R: TransportRead + Send + 'static, W: TransportWrite + Send + 'static>(
    read: R,
    write: W,
    is_a: bool,
) where
    R::Error: Send,
    W::Error: Send,
{
    let tx_alloc = Box::leak(Box::new(BufferPool::<
        ThreadModeRawMutex,
        Vec<u8, CHUNK_LEN_MAX>,
        8,
    >::new(array_new!(Vec::new(), 8))));
    let tx_ch = Box::leak(Box::new(channel::Channel::<
        ThreadModeRawMutex,
        BufferGuard<ThreadModeRawMutex, Vec<u8, CHUNK_LEN_MAX>>,
        8,
    >::new()));
    let tx_ch_tx = tx_ch.sender();
    let tx_ch_rx = tx_ch.receiver();

    let acks_to_send = Box::leak(Box::new(
        channel::Channel::<ThreadModeRawMutex, Ack, 8>::new(),
    ));
    let acks_received = Box::leak(Box::new(
        channel::Channel::<ThreadModeRawMutex, Ack, 8>::new(),
    ));
    let rx_alloc = Box::leak(Box::new(
        BufferPool::<ThreadModeRawMutex, [u8; 4096], 4>::new([[0u8; 4096]; 4]),
    ));
    let rx_queue = Box::leak(Box::new(channel::Channel::<
        ThreadModeRawMutex,
        MappedBufferGuard<ThreadModeRawMutex, [u8]>,
        4,
    >::new()));

    tokio::task::spawn_local(run_write(
        Box::leak(Box::new(write)),
        Box::leak(Box::new(tx_ch_rx)),
        Box::leak(Box::new(acks_to_send.receiver())),
        Box::leak(Box::new(SoftwareCrc)),
    ));

    tokio::task::spawn_local(run_read(
        Box::leak(Box::new(read)),
        Box::leak(Box::new(SoftwareCrc)),
        rx_alloc,
        rx_queue.sender(),
        acks_to_send.sender(),
        acks_received.sender(),
    ));

    tokio::task::spawn_local(async {
        loop {
            let buf = rx_queue.receive().await;
            println!("received buffer {:?}", &*buf);
        }
    });

    let mut sender = SecureSerialSender::new(
        tx_alloc,
        tx_ch_tx,
        acks_received.receiver(),
        Duration::from_millis(1000),
        3,
    );

    loop {
        Timer::after_secs(1).await;

        if is_a {
            sender.write_packet(b"test hello").await.unwrap();
        }
        Timer::after_secs(4).await;
    }
}

// ------------------------------------------------------------------------------------
pub struct SoftwareCrc;

impl CrcDevice for SoftwareCrc {
    async fn crc(&mut self, data: &[u8]) -> u32 {
        const SW_CRC: Crc<u32> = Crc::<u32>::new(&CRC_32_ISO_HDLC);
        SW_CRC.checksum(data)
    }
}

pub struct PipeTransportRead<'p, M: RawMutex, const N: usize> {
    inner: Reader<'p, M, N>,
}

impl<'p, M: RawMutex, const N: usize> TransportRead for PipeTransportRead<'p, M, N> {
    type Error = ();

    async fn read(&mut self, data: &mut [u8]) -> Result<usize, Self::Error> {
        let res = self.inner.read(data).await;
        Ok(res)
    }
}

pub struct PipeTransportWrite<'p, M: RawMutex, const N: usize> {
    inner: Writer<'p, M, N>,
}

impl<'p, M: RawMutex, const N: usize> TransportWrite for PipeTransportWrite<'p, M, N> {
    type Error = ();

    async fn write(&mut self, data: &[u8]) -> Result<(), Self::Error> {
        let mut remaining = data;
        println!("write {} bytes: {:02x?}", data.len(), data);
        while !remaining.is_empty() {
            let n = self.inner.write(remaining).await;
            remaining = &remaining[n..];
        }
        Ok(())
    }
}

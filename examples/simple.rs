use crc::{CRC_32_ISO_HDLC, Crc};
use embassy_sync::{
    blocking_mutex::raw::{RawMutex, ThreadModeRawMutex},
    pipe::{Pipe, Reader, Writer},
};
use embassy_time::{Duration, Timer};
use secure_serial::{
    CrcDevice, SecureSerialResources, SecureSerialResourcesDefault, SecureSerialSender,
    TransportRead, TransportWrite, run_read, run_write,
};
use static_cell::ConstStaticCell;

static PIPE_A_TO_B: ConstStaticCell<Pipe<ThreadModeRawMutex, 200>> =
    ConstStaticCell::new(Pipe::new());
static PIPE_B_TO_A: ConstStaticCell<Pipe<ThreadModeRawMutex, 200>> =
    ConstStaticCell::new(Pipe::new());

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
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
        })
        .await;
}

async fn run_one_side<R: TransportRead + Send + 'static, W: TransportWrite + Send + 'static>(
    read: R,
    write: W,
    is_a: bool,
) where
    R::Error: Send,
    W::Error: Send,
{
    let res: &'static SecureSerialResources<ThreadModeRawMutex, 8, 4, 4096> = &*Box::leak(
        Box::new(SecureSerialResourcesDefault::<ThreadModeRawMutex>::new()),
    );

    tokio::task::spawn_local(run_write(
        Box::leak(Box::new(write)),
        Box::leak(Box::new(res.tx_chunks_receiver())),
        Box::leak(Box::new(res.acks_to_wire_receiver())),
        Box::leak(Box::new(SoftwareCrc)),
    ));

    tokio::task::spawn_local(run_read(
        Box::leak(Box::new(read)),
        Box::leak(Box::new(SoftwareCrc)),
        res.rx_pool(),
        res.rx_complete_sender(),
        res.acks_to_wire_sender(),
        res.acks_from_peer_sender(),
    ));

    tokio::task::spawn_local(async move {
        let rx = res.rx_complete_receiver();
        loop {
            let buf = rx.receive().await;
            println!("received buffer {:?}", &*buf);
        }
    });

    let mut sender = SecureSerialSender::new(
        res.tx_pool(),
        res.tx_chunks_sender(),
        res.acks_from_peer_receiver(),
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

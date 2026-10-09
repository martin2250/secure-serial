use secure_serial::{SoftwareCrc, TransportRx, TransportTx};

fn main() {
    let mut crc = SoftwareCrc;
    let mut tx = TransportTx::<32>::new();
    let mut rx = TransportRx::<32>::new();

    tx.payload_mut()[..2].copy_from_slice(b"OK");
    let packet = tx.finish(2, &mut crc).unwrap();

    // A serial connection may deliver the packet in several chunks.
    for chunk in packet.chunks(3) {
        let mut parser = rx.process(chunk, &mut crc);
        while let Some(payload) = parser.next() {
            assert_eq!(payload, b"OK");
            println!("Received: {payload:?}");
        }
    }
}

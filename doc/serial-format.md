# What this does

This library wraps a serial connection between two devices.
Each device gets one "write" and one "read" endpoint.
packets are transmitted in smaller chunks, protected by checksums.
on the receiving end, the chunks are re-assembled and confirmed to the sender

# Packet Format

All communication happens in chunks of 32, 64 or 128 bytes.
The packet id increments with every packet.

## data packet
packet_id: u16
length: u8
offset: u32


## Ack Packet

repeat for n subpackets:

packet_id: u16
offset: u32

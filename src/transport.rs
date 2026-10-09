use crate::CrcDevice;

const PREAMBLE: [u8; 2] = [0x55, 0x99];
const PACKET_HEADER_LEN: usize = PREAMBLE.len() + 2; // preamble (2) + len (2)
const PACKET_CRC_LEN: usize = 4;
const PACKET_LEN_MIN: usize = PACKET_HEADER_LEN + PACKET_CRC_LEN;

pub struct TransportRx<const BUFFER: usize = 512> {
    buffer: [u8; BUFFER],
    count: usize,
}

impl<const BUFFER: usize> TransportRx<BUFFER> {
    pub fn new() -> Self {
        Self {
            buffer: [0; _],
            count: 0,
        }
    }

    pub fn process<'a, CRC: CrcDevice>(
        &'a mut self,
        new_data: &'a [u8],
        crc: &'a mut CRC,
    ) -> IterRx<'a, CRC, BUFFER> {
        IterRx {
            new_data,
            transport: self,
            crc: crc,
        }
    }
}

pub struct IterRx<'a, CRC: CrcDevice, const BUFFER: usize = 512> {
    new_data: &'a [u8],
    transport: &'a mut TransportRx<BUFFER>,
    crc: &'a mut CRC,
}

impl<'a, CRC: CrcDevice, const BUFFER: usize> Iterator for IterRx<'a, CRC, BUFFER> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.new_data.len() == 0 {
            return None;
        }

        if self.transport.count == 0 {
            // transport count > 0 --> there is already a valid packet beginning in the buffer
            None
        } else {
            // transport count == 0 --> search for a valid packet in the new data
            loop {
                // find the start of the next potential packet
                let Some(index_preamble) = self.new_data.iter().position(|&b| b == PREAMBLE[0])
                else {
                    // no header found in new_data, discard all of it
                    return None;
                };
                // put header into first byte of new_data
                self.new_data = &self.new_data[index_preamble..];
                // check preamble[1] invalid -> try parsing again at next byte
                if self.new_data.get(1).is_some_and(|&b| b != PREAMBLE[1]) {
                    self.new_data = &self.new_data[1..];
                    continue;
                }
                let len_opt = rx_get_len(self.new_data);
                // check packet too long for buffer -> try parsing again at next byte
                if len_opt.is_some_and(|len| len > BUFFER - PACKET_LEN_MIN) {
                    self.new_data = &self.new_data[1..];
                    continue;
                }
                // set to None if we haven't received enough bytes yet
                let len_opt = len_opt.filter(|&len| len + PACKET_LEN_MIN <= self.new_data.len());
                // None --> copy remaining data to transport
                let Some(len) = len_opt else {
                    // packet too short, copy to transport and exit
                    self.transport.buffer[..self.new_data.len()].copy_from_slice(self.new_data);
                    self.transport.count = self.new_data.len();
                    return None;
                };
                // check CRC
                let mut crc_buf = [0; 4];
                crc_buf
                    .copy_from_slice(&self.new_data[PACKET_HEADER_LEN + len..][..PACKET_CRC_LEN]);
                let crc_recv = u32::from_le_bytes(crc_buf);
                let crc_calc = self.crc.crc(&self.new_data[..PACKET_HEADER_LEN + len]);
                // invalid crc -> try parsing again at next byte
                if crc_recv != crc_calc {
                    self.new_data = &self.new_data[1..];
                    continue;
                }
                let payload = &self.new_data[PACKET_HEADER_LEN..][..len];
                self.new_data = &self.new_data[PACKET_LEN_MIN + len..];
                break Some(payload);
            }
        }
    }
}

fn rx_get_len(data: &[u8]) -> Option<usize> {
    // packet too short to contain len
    if data.len() < 4 {
        return None;
    }
    let mut buffer = [0; 2];
    buffer.copy_from_slice(&data[2..4]);
    Some(u16::from_le_bytes(buffer) as usize)
}

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

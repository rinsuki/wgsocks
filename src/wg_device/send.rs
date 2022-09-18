use super::WGDevice;

pub struct TxToken<'a> {
    pub device: &'a WGDevice,
}

impl<'a> smoltcp::phy::TxToken for TxToken<'a> {
    fn consume<R, F>(self, _timestamp: smoltcp::time::Instant, len: usize, f: F) -> smoltcp::Result<R>
    where
        F: FnOnce(&mut [u8]) -> smoltcp::Result<R> {
        let mut send_buf = vec![0u8; len];
        let result = f(&mut send_buf);
        match result {
            Ok(_) => {
                self.device.send(&send_buf);
            },
            Err(e) => {
                println!("error {:?}", e);
            }
        }
        result
    }
}
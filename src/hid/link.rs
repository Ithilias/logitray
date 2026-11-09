//! The two operations logitray needs from an open HID collection, as a trait so
//! the receiver worker can run against simulated receivers in tests (see
//! `sim`). hidapi's [`HidDevice`] is the only real implementation.

use anyhow::Result;
use hidapi::HidDevice;

pub trait HidLink {
    /// Write one report (report id first). Returns the bytes written.
    fn write(&self, data: &[u8]) -> Result<usize>;
    /// Read one report, waiting up to `timeout_ms`. Returns 0 on timeout.
    fn read_timeout(&self, buf: &mut [u8], timeout_ms: i32) -> Result<usize>;
}

impl HidLink for HidDevice {
    fn write(&self, data: &[u8]) -> Result<usize> {
        Ok(HidDevice::write(self, data)?)
    }

    fn read_timeout(&self, buf: &mut [u8], timeout_ms: i32) -> Result<usize> {
        Ok(HidDevice::read_timeout(self, buf, timeout_ms)?)
    }
}

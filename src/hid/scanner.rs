use anyhow::{Context, Result};
use hidapi::{HidApi, HidDevice};

pub const LOGITECH_VID: u16 = 0x046D;

/// The HID++ vendor interface on Logitech receivers uses this usage page.
const HIDPP_USAGE_PAGE: u16 = 0xFF00;

/// A discovered Logitech HID++ receiver interface ready to be opened.
#[derive(Clone, Debug)]
pub struct ReceiverPath {
    pub pid: u16,
    pub path: std::ffi::CString,
}

/// HID++ usage for the long-report (0x11, 20-byte) collection. Modern devices
/// reply on this channel, so we must talk to it rather than the short (0x01) one.
const HIDPP_LONG_USAGE: u16 = 0x0002;

/// Scan for all Logitech HID++ receiver interfaces.
///
/// We look for any device with VID 0x046D and usage_page 0xFF00.
/// This covers Unifying receivers, LIGHTSPEED nano-receivers, and
/// Bolt receivers without needing a hardcoded PID list.
///
/// On Windows the HID++ command interface (MI_02) exposes two top-level
/// collections: usage 0x0001 (short, 0x10 reports) and usage 0x0002 (long,
/// 0x11 reports) as separate device paths. We prefer the long one because
/// wireless devices answer on the long channel; a short request to the device
/// gets no reply on the short handle. We deduplicate by PID.
pub fn scan_receivers(api: &HidApi) -> Vec<ReceiverPath> {
    // pid -> (usage, path); we keep the best interface seen so far per receiver.
    let mut best: std::collections::HashMap<u16, (u16, std::ffi::CString)> =
        std::collections::HashMap::new();

    for info in api.device_list() {
        if info.vendor_id() != LOGITECH_VID {
            continue;
        }
        if info.usage_page() != HIDPP_USAGE_PAGE {
            continue;
        }
        let pid = info.product_id();
        let usage = info.usage();
        match best.get(&pid) {
            // Already have the preferred long interface — keep it.
            Some((u, _)) if *u == HIDPP_LONG_USAGE => {}
            // Otherwise take this one if it's the long interface or we have nothing better.
            _ => {
                best.insert(pid, (usage, info.path().to_owned()));
            }
        }
    }

    best.into_iter()
        .map(|(pid, (_usage, path))| ReceiverPath { pid, path })
        .collect()
}

pub fn open_receiver(api: &HidApi, receiver: &ReceiverPath) -> Result<HidDevice> {
    api.open_path(receiver.path.as_c_str())
        .with_context(|| format!("failed to open receiver {:04X}", receiver.pid))
}

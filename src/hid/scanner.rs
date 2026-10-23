use anyhow::{Context, Result};
use hidapi::{HidApi, HidDevice};

pub const LOGITECH_VID: u16 = 0x046D;

/// The HID++ vendor interface on Logitech receivers uses this usage page.
const HIDPP_USAGE_PAGE: u16 = 0xFF00;

/// A discovered Logitech HID++ receiver, with both vendor collections it exposes.
///
/// On Windows a receiver presents the HID++ command interface (MI_02) as two
/// top-level collections at separate device paths: usage 0x0001 (short, 0x10
/// reports) and usage 0x0002 (long, 0x11 reports). HID++ 2.0 device traffic
/// (ping, enumeration, battery) goes on the long collection; the HID++ 1.0
/// receiver registers and the device-connection notification live on the short
/// collection (confirmed on C547 via `--diag`). We keep both.
#[derive(Clone, Debug)]
pub struct ReceiverPath {
    pub pid: u16,
    /// Physical interface identity, shared by its short and long collections.
    pub key: String,
    /// usage 0x0002 — HID++ 2.0 device traffic. Always present (selection key).
    pub long_path: std::ffi::CString,
    /// usage 0x0001 — receiver registers + connection notifications. Absent on
    /// receivers that don't expose a short vendor collection.
    pub short_path: Option<std::ffi::CString>,
}

/// HID++ usage for the long-report (0x11, 20-byte) collection. Modern devices
/// reply on this channel, so we must talk to it rather than the short (0x01) one.
const HIDPP_LONG_USAGE: u16 = 0x0002;
/// HID++ usage for the short-report (0x10, 7-byte) collection — receiver
/// registers and notifications.
const HIDPP_SHORT_USAGE: u16 = 0x0001;

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
/// gets no reply on the short handle. Collections are grouped by their physical interface path.
pub fn scan_receivers(api: &HidApi) -> Vec<ReceiverPath> {
    // (PID, physical interface identity) -> (long?, short?) collected across the receiver's vendor interfaces.
    let mut found: std::collections::HashMap<
        (u16, String),
        (Option<std::ffi::CString>, Option<std::ffi::CString>),
    > = std::collections::HashMap::new();

    for info in api.device_list() {
        if info.vendor_id() != LOGITECH_VID || info.usage_page() != HIDPP_USAGE_PAGE {
            continue;
        }
        let entry = found
            .entry((info.product_id(), receiver_identity(info.path().to_bytes())))
            .or_default();
        match info.usage() {
            HIDPP_LONG_USAGE => entry.0 = Some(info.path().to_owned()),
            HIDPP_SHORT_USAGE => entry.1 = Some(info.path().to_owned()),
            _ => {}
        }
    }

    found
        .into_iter()
        // The long collection is required (device traffic); without it the
        // receiver is unusable for our purposes.
        .filter_map(|((pid, key), (long, short))| {
            long.map(|long_path| ReceiverPath {
                pid,
                key,
                long_path,
                short_path: short,
            })
        })
        .collect()
}

/// Open the long (0x0002) collection — HID++ 2.0 device traffic.
pub fn open_receiver(api: &HidApi, receiver: &ReceiverPath) -> Result<HidDevice> {
    api.open_path(receiver.long_path.as_c_str())
        .with_context(|| format!("failed to open receiver {:04X}", receiver.pid))
}

/// Open the short (0x0001) collection — receiver registers + notifications.
/// Errors if the receiver exposes no short collection.
pub fn open_notifier(api: &HidApi, receiver: &ReceiverPath) -> Result<HidDevice> {
    let path = receiver
        .short_path
        .as_ref()
        .with_context(|| format!("receiver {:04X} has no short collection", receiver.pid))?;
    api.open_path(path.as_c_str())
        .with_context(|| format!("failed to open receiver {:04X} notifier", receiver.pid))
}

// Normalize Windows collection components (&colXX and its instance ordinal).
// Preserve the instance and interface portions, so equal PIDs remain distinct.
fn receiver_identity(path: &[u8]) -> String {
    let mut key = String::from_utf8_lossy(path).to_ascii_lowercase();
    if let Some(start) = key.find("&col") {
        let end = start + 6;
        let collection = key
            .get(start + 4..end)
            .and_then(|s| u16::from_str_radix(s, 16).ok());
        if key
            .get(start + 4..end)
            .is_some_and(|s| s.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            // Windows also encodes the zero-based collection ordinal at the
            // end of the device instance component (&0000, &0001, ...).
            let instance_end = key[end..].find('#').and_then(|first| {
                let instance_start = end + first + 1;
                key[instance_start..].find('#').map(|n| instance_start + n)
            });
            if let (Some(collection), Some(instance_end)) = (collection, instance_end) {
                let suffix = format!("&{:04x}", collection.saturating_sub(1));
                if key[..instance_end].ends_with(&suffix) {
                    key.replace_range(instance_end - suffix.len()..instance_end, "");
                }
            }
            key.replace_range(start..end, "");
        }
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn collection_identity_preserves_physical_receiver() {
        let short = b"hid#vid_046d&pid_c547&mi_02&col01#instance-a#{guid}";
        let long = b"hid#vid_046d&pid_c547&mi_02&col02#instance-a#{guid}";
        let other = b"hid#vid_046d&pid_c547&mi_02&col02#instance-b#{guid}";
        assert_eq!(receiver_identity(short), receiver_identity(long));
        assert_ne!(receiver_identity(long), receiver_identity(other));
        let short = b"hid#vid_046d&pid_c094&mi_02&col01#8&236d0d0a&0&0000#{guid}";
        let long = b"hid#vid_046d&pid_c094&mi_02&col02#8&236d0d0a&0&0001#{guid}";
        let other = b"hid#vid_046d&pid_c094&mi_02&col02#8&12345678&0&0001#{guid}";
        assert_eq!(receiver_identity(short), receiver_identity(long));
        assert_ne!(receiver_identity(long), receiver_identity(other));
    }
}

//! HID++ 1.0 *receiver* register access: enabling notifications, asking the
//! receiver to re-announce its connected devices, and parsing the device-
//! connection notification (sub-id 0x41).
//!
//! These talk to the receiver itself (device index 0xFF) using the HID++ 1.0
//! register frame, which differs from the HID++ 2.0 feature frame in
//! [`crate::hid::protocol`]: byte[2] is a *sub-id* (0x80 set / 0x81 get) and
//! byte[3] is the *register address* (no software-id nibble).
//!
//! These run on the receiver's **short** vendor collection (usage 0x0001) using
//! short (0x10) reports — confirmed on a `C547` LIGHTSPEED receiver via `--diag`:
//! the short interface answers register reads/writes and pushes the connection
//! notification, while the long (0x0002) interface carries HID++ 2.0 device
//! traffic. Byte layouts follow Solaar's `lib/logitech_receiver/hidpp10.py` and
//! `receiver.py`. Bolt pairing sub-registers differ and are left a marked stub.

use crate::hid::protocol::{classify, normalize_report, ReportClass, SHORT_REPORT_ID};
use anyhow::{bail, Context, Result};
use hidapi::HidDevice;
use std::thread;
use std::time::Duration;

/// The receiver addresses itself at device index 0xFF.
const RECEIVER_INDEX: u8 = 0xFF;

/// HID++ 1.0 register sub-ids.
const SUB_SET_REGISTER: u8 = 0x80;
const SUB_GET_REGISTER: u8 = 0x81;

/// Registers we use.
const REG_NOTIFICATIONS: u8 = 0x00;
const REG_CONNECTION_STATE: u8 = 0x02;

/// Notification-flag bits (24-bit field written to register 0x00), big-endian.
/// Only the bits relevant to "tell me when a wireless device comes/goes and when
/// its battery changes" — see Solaar `hidpp10_constants.NotificationFlag`.
const NOTIF_WIRELESS: u32 = 0x000100;
const NOTIF_SOFTWARE_PRESENT: u32 = 0x000800;
const NOTIF_BATTERY_STATUS: u32 = 0x100000;

/// Value written to the connection-state register (0x02) to make the receiver
/// re-emit a connection notification for every currently-paired device.
const ANNOUNCE_ALL: u8 = 0x02;

/// How long to wait for a register set/get acknowledgement. Setup is best-effort
/// — a receiver that ignores these (some gaming nano receivers) just means we
/// fall back to discovery-by-ping and the periodic safety re-read.
const REGISTER_ACK_TIMEOUT_MS: i32 = 600;
const REGISTER_ACK_TRIES: usize = 3;

/// A parsed HID++ 1.0 device-connection notification (sub-id 0x41).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnectionNotification {
    /// Receiver slot (1..=6) the notification refers to.
    pub device_index: u8,
    /// Whether the wireless link is up (device present) or down (device gone).
    pub link_established: bool,
    /// Wireless product id of the paired device. Best-effort: the byte order of
    /// this field is receiver-family dependent (Bolt differs); treat as opaque
    /// and verify against `--diag` before relying on it as a stable key.
    pub wpid: u16,
}

/// Build and write a HID++ 1.0 register request as a 7-byte short report,
/// without waiting for a reply. Used fire-and-forget when the matching reply (or
/// the notifications the request triggers) will be consumed by a reader loop.
fn write_register(dev: &HidDevice, set: bool, register: u8, params: [u8; 3]) -> Result<()> {
    let sub_id = if set {
        SUB_SET_REGISTER
    } else {
        SUB_GET_REGISTER
    };
    let mut request = [0u8; 7];
    request[0] = SHORT_REPORT_ID;
    request[1] = RECEIVER_INDEX;
    request[2] = sub_id;
    request[3] = register;
    request[4..7].copy_from_slice(&params);
    dev.write(&request).context("register write failed")?;
    Ok(())
}

/// Send a HID++ 1.0 register request and wait for its matching acknowledgement,
/// tolerating (and discarding) any unsolicited notifications that interleave.
///
/// `set` chooses sub-id 0x80 (write) vs 0x81 (read). Requests go out as short
/// (0x10) reports on the receiver's short collection. Returns the normalized
/// 7-byte acknowledgement.
fn register_request(dev: &HidDevice, set: bool, register: u8, params: [u8; 3]) -> Result<[u8; 7]> {
    let sub_id = if set {
        SUB_SET_REGISTER
    } else {
        SUB_GET_REGISTER
    };

    for attempt in 0..REGISTER_ACK_TRIES {
        write_register(dev, set, register, params)?;
        loop {
            let mut buf = [0u8; 21];
            let n = dev
                .read_timeout(&mut buf, REGISTER_ACK_TIMEOUT_MS)
                .context("register read failed")?;
            if n == 0 {
                break; // timeout — retry the whole request
            }
            let Some(report) = normalize_report(&buf, n) else {
                continue;
            };
            match classify(&report) {
                ReportClass::Error => bail!("receiver rejected register 0x{register:02X}"),
                ReportClass::RegisterReply { sub_id: got } if got == sub_id => return Ok(report),
                // Anything else (a connection/battery notification, or an ack for
                // a different register) is not our reply — keep reading.
                _ => continue,
            }
        }
        if attempt + 1 < REGISTER_ACK_TRIES {
            thread::sleep(Duration::from_millis(50));
        }
    }
    bail!("no acknowledgement for register 0x{register:02X}")
}

/// Tell the receiver to push wireless connect/disconnect and battery
/// notifications, and to treat us as the managing software. Best-effort.
pub fn enable_notifications(dev: &HidDevice) -> Result<()> {
    let flags = NOTIF_WIRELESS | NOTIF_SOFTWARE_PRESENT | NOTIF_BATTERY_STATUS;
    let params = [(flags >> 16) as u8, (flags >> 8) as u8, flags as u8];
    register_request(dev, true, REG_NOTIFICATIONS, params)?;
    Ok(())
}

/// Ask the receiver to re-announce every currently-connected device, which makes
/// it emit one connection notification (sub-id 0x41) per device (and an error
/// notification per empty slot). Fire-and-forget: the triggered notifications —
/// including the `0x41` arrivals we care about — are consumed by the caller's
/// read loop, so we must *not* wait for the register ack here (that would eat the
/// connection notification that races ahead of it). Best-effort.
pub fn poke_announce(dev: &HidDevice) -> Result<()> {
    write_register(dev, true, REG_CONNECTION_STATE, [ANNOUNCE_ALL, 0x00, 0x00])
}

/// Number of devices currently paired to the receiver, via register 0x02 (get).
/// Best-effort; not all receivers answer.
pub fn connected_count(dev: &HidDevice) -> Result<u8> {
    let reply = register_request(dev, false, REG_CONNECTION_STATE, [0x00, 0x00, 0x00])?;
    // Reply payload byte[1] (report[5]) carries the connected-device count —
    // confirmed on C547: GET reg 0x02 returned `10 FF 81 02 00 01 00` with one
    // device paired.
    Ok(reply[5])
}

/// Parse a normalized 7-byte connection notification (sub-id 0x41).
///
/// Layout (after report-id): `[1]` slot, `[2]` = 0x41, `[3]` device-info byte,
/// `[4]` link-status flags (bit 6 set ⇒ link *down*), `[5..7]` WPID. Returns
/// `None` if this isn't a 0x41 report. See Solaar `notifications.py`.
pub fn parse_connection(report: &[u8; 7]) -> Option<ConnectionNotification> {
    if report[2] != 0x41 {
        return None;
    }
    let flags = report[4];
    Some(ConnectionNotification {
        device_index: report[1],
        // Bit 6 (0x40) set means the link is *not* established (device left).
        link_established: (flags & 0x40) == 0,
        // WPID is the last two payload bytes, high byte last: C547 reported
        // `... 99 40` for a 0x4099 device. (Bolt reverses this — TODO(bolt).)
        wpid: ((report[6] as u16) << 8) | report[5] as u16,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid::protocol::SHORT_REPORT_ID;

    #[test]
    fn connection_link_up_and_down() {
        // Real C547 capture: `10 01 41 11 32 99 40` — slot 1, link up, WPID 0x4099.
        let up = [SHORT_REPORT_ID, 0x01, 0x41, 0x11, 0x32, 0x99, 0x40];
        let n = parse_connection(&up).expect("0x41 should parse");
        assert_eq!(n.device_index, 0x01);
        assert!(n.link_established);
        assert_eq!(n.wpid, 0x4099);

        // Link gone: flags bit 6 (0x40) set.
        let down = [SHORT_REPORT_ID, 0x03, 0x41, 0x11, 0x72, 0x99, 0x40];
        let n = parse_connection(&down).expect("0x41 should parse");
        assert_eq!(n.device_index, 0x03);
        assert!(!n.link_established);
    }

    #[test]
    fn non_connection_report_is_none() {
        let battery_event = [0x11, 0x01, 0x06, 0x00, 50, 0, 0];
        assert!(parse_connection(&battery_event).is_none());
    }

    #[test]
    fn notification_flag_bytes() {
        // wireless | software-present | battery = 0x100900, big-endian.
        let flags = NOTIF_WIRELESS | NOTIF_SOFTWARE_PRESENT | NOTIF_BATTERY_STATUS;
        let params = [(flags >> 16) as u8, (flags >> 8) as u8, flags as u8];
        assert_eq!(params, [0x10, 0x09, 0x00]);
    }
}

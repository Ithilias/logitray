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
//!
//! The same frame, addressed to a slot instead of 0xFF, reads the battery
//! registers of paired HID++ 1.0 devices, which have no HID++ 2.0 feature table.
//! Their replies also arrive on the short collection.

use crate::hid::link::HidLink;
use crate::hid::protocol::{normalize_report, SHORT_REPORT_ID};
use anyhow::{bail, Context, Result};
use std::thread;
use std::time::Duration;

/// The receiver addresses itself at device index 0xFF.
pub const RECEIVER_INDEX: u8 = 0xFF;

/// HID++ 1.0 register sub-ids.
pub const SUB_SET_REGISTER: u8 = 0x80;
pub const SUB_GET_REGISTER: u8 = 0x81;

/// Registers we use.
pub const REG_NOTIFICATIONS: u8 = 0x00;
const REG_CONNECTION_STATE: u8 = 0x02;

/// Notification-flag bits (24-bit field written to register 0x00), big-endian.
/// Only the bits relevant to "tell me when a wireless device comes/goes and when
/// its battery changes" — see Solaar `hidpp10_constants.NotificationFlag`.
const NOTIF_WIRELESS: u32 = 0x000100;
const NOTIF_SOFTWARE_PRESENT: u32 = 0x000800;
const NOTIF_BATTERY_STATUS: u32 = 0x100000;

/// Battery registers on a paired HID++ 1.0 *device* (e.g. the original K800,
/// WPID 0x2010). 0x0D holds a percentage, 0x07 a coarse level code. Which one a
/// device has is not discoverable, so Solaar tries 0x0D first, then 0x07.
pub const REG_BATTERY_CHARGE: u8 = 0x0D;
pub const REG_BATTERY_STATUS: u8 = 0x07;
pub const BATTERY_REGISTERS: [u8; 2] = [REG_BATTERY_CHARGE, REG_BATTERY_STATUS];

/// HID++ 1.0 error sub-id.
const SUB_ERROR: u8 = 0x8F;

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
fn write_register(dev: &dyn HidLink, set: bool, register: u8, params: [u8; 3]) -> Result<()> {
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
fn register_request(
    dev: &dyn HidLink,
    set: bool,
    register: u8,
    params: [u8; 3],
) -> Result<[u8; 7]> {
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
            // Match the receiver's own address and the register: devices'
            // register traffic shares this collection.
            match match_register_answer(RECEIVER_INDEX, sub_id, register, &report) {
                RegisterAnswer::Value(_) => return Ok(report),
                RegisterAnswer::Rejected(code) => {
                    return Err(RegisterRejected { register, code }.into())
                }
                // Anything else (a connection/battery notification, or an ack for
                // a different register) is not our reply — keep reading.
                RegisterAnswer::Unrelated => continue,
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
pub fn enable_notifications(dev: &dyn HidLink) -> Result<()> {
    register_request(dev, true, REG_NOTIFICATIONS, notification_flags())?;
    Ok(())
}

/// The receiver notification flags [`enable_notifications`] writes, as the
/// big-endian register value.
pub fn notification_flags() -> [u8; 3] {
    let flags = NOTIF_WIRELESS | NOTIF_SOFTWARE_PRESENT | NOTIF_BATTERY_STATUS;
    [(flags >> 16) as u8, (flags >> 8) as u8, flags as u8]
}

/// Ask the receiver to re-announce every currently-connected device, which makes
/// it emit one connection notification (sub-id 0x41) per device (and an error
/// notification per empty slot). Fire-and-forget: the triggered notifications —
/// including the `0x41` arrivals we care about — are consumed by the caller's
/// read loop, so we must *not* wait for the register ack here (that would eat the
/// connection notification that races ahead of it). Best-effort.
pub fn poke_announce(dev: &dyn HidLink) -> Result<()> {
    write_register(dev, true, REG_CONNECTION_STATE, [ANNOUNCE_ALL, 0x00, 0x00])
}

/// Number of devices currently paired to the receiver, via register 0x02 (get).
/// Best-effort; not all receivers answer.
pub fn connected_count(dev: &dyn HidLink) -> Result<u8> {
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

/// Short (0x10) request that reads `register` from the HID++ 1.0 device in
/// `slot`. Same frame as a receiver register read, addressed to the slot.
pub fn device_register_read(slot: u8, register: u8) -> [u8; 7] {
    register_frame(slot, SUB_GET_REGISTER, register, [0; 3])
}

/// Short (0x10) HID++ 1.0 register request: `sub_id` 0x80 set / 0x81 get of
/// `register` at `slot` (a device slot, or [`RECEIVER_INDEX`]).
pub fn register_frame(slot: u8, sub_id: u8, register: u8, params: [u8; 3]) -> [u8; 7] {
    [
        SHORT_REPORT_ID,
        slot,
        sub_id,
        register,
        params[0],
        params[1],
        params[2],
    ]
}

/// The notification register (0x00) of a HID++ 1.0 *device*; the battery bit
/// makes it push its battery register when the level changes. Solaar sets this
/// on each 1.0 device in addition to the receiver's flags.
pub const REG_DEVICE_NOTIFICATIONS: u8 = REG_NOTIFICATIONS;

/// `value` (as read from a device's register 0x00) with the battery bit set, or
/// `None` when it is already set. Keeps whatever other bits are on, so flags
/// another program enabled survive.
pub fn with_battery_notifications(value: [u8; 3]) -> Option<[u8; 3]> {
    let bit = (NOTIF_BATTERY_STATUS >> 16) as u8;
    (value[0] & bit == 0).then_some([value[0] | bit, value[1], value[2]])
}

/// Answer to a HID++ 2.0 request (feature index `feature_index`, byte 3 =
/// `function_byte`) that arrived as a HID++ 1.0 error on the short collection,
/// as `Some(error code)`. A 1.0 device rejects a 2.0 request as an unknown
/// sub-id (0x01); the receiver answers for an empty slot with 0x08.
pub fn hidpp20_request_error(
    slot: u8,
    feature_index: u8,
    function_byte: u8,
    report: &[u8; 7],
) -> Option<u8> {
    (report[1] == slot
        && report[2] == SUB_ERROR
        && report[3] == feature_index
        && report[4] == function_byte)
        .then_some(report[5])
}

/// HID++ 1.0 error codes (Solaar `hidpp10_constants.ErrorCode`).
pub const ERR_INVALID_SUBID: u8 = 0x01;
pub const ERR_INVALID_ADDRESS: u8 = 0x02;
pub const ERR_CONNECTION_REQUEST_FAILED: u8 = 0x04;
pub const ERR_BUSY: u8 = 0x07;
pub const ERR_UNKNOWN_DEVICE: u8 = 0x08;
pub const ERR_RESOURCE_ERROR: u8 = 0x09;

/// The device is paired but cannot be reached right now (asleep, out of range):
/// how Solaar's ping reads 0x04 and 0x09.
pub fn is_unreachable(code: u8) -> bool {
    matches!(code, ERR_CONNECTION_REQUEST_FAILED | ERR_RESOURCE_ERROR)
}

/// A register request answered with a HID++ 1.0 error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegisterRejected {
    pub register: u8,
    pub code: u8,
}

impl std::fmt::Display for RegisterRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "register 0x{:02X} rejected (error 0x{:02X})",
            self.register, self.code
        )
    }
}

impl std::error::Error for RegisterRejected {}

/// What a normalized report on the short collection means for a pending
/// register request (`sub_id` 0x80 set / 0x81 get) to `register` at `slot`
/// (a device slot, or [`RECEIVER_INDEX`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegisterAnswer {
    /// The answer: `[4..7]` of the reply (the value for a get).
    Value([u8; 3]),
    /// The device (or the receiver on its behalf) rejected the request; the
    /// byte is the HID++ 1.0 error code.
    Rejected(u8),
    /// Some other report, not ours.
    Unrelated,
}

pub fn match_register_answer(
    slot: u8,
    sub_id: u8,
    register: u8,
    report: &[u8; 7],
) -> RegisterAnswer {
    if report[1] != slot {
        return RegisterAnswer::Unrelated;
    }
    if report[2] == sub_id && report[3] == register {
        return RegisterAnswer::Value([report[4], report[5], report[6]]);
    }
    // Error frame: `8F <sub-id> <register> <code>`.
    if report[2] == SUB_ERROR && report[3] == sub_id && report[4] == register {
        return RegisterAnswer::Rejected(report[5]);
    }
    RegisterAnswer::Unrelated
}

/// True for a battery notification a HID++ 1.0 device pushes on its own when
/// its level changes (enabled by [`NOTIF_BATTERY_STATUS`]): the sub-id is the
/// register number and the payload is that register's value.
pub fn is_battery_notification(report: &[u8; 7]) -> bool {
    (1..=6).contains(&report[1]) && BATTERY_REGISTERS.contains(&report[2])
}

/// Decode a battery register value into `(percent, charging)`, following
/// Solaar's `hidpp10.parse_battery_status`. The percentage is `None` when the
/// value carries no level.
///
/// Register 0x07 only knows four levels, so the percentage is the same coarse
/// approximation Solaar shows (full 90, good 50, low 20, critical 5). Level 0
/// is empty (0%), but with the charger's "full" bits and no level it is a full
/// battery (100%), and with only charging bits Solaar reads it as "charging,
/// level unknown". Any other code is unknown too, rather than Solaar's empty,
/// so a value we do not understand never raises a low-battery warning.
pub fn parse_battery_register(register: u8, value: [u8; 3]) -> Result<(Option<u8>, bool)> {
    match register {
        REG_BATTERY_CHARGE => {
            let percent = value[0].min(100);
            let charging = value[2] & 0xF0 == 0x50;
            Ok((Some(percent), charging))
        }
        REG_BATTERY_STATUS => {
            // Solaar checks "recharging" (0x21) before "full" (0x22).
            let charging = value[1] & 0x21 == 0x21;
            let full = !charging && value[1] & 0x22 == 0x22;
            let percent = match value[0] {
                7 => Some(90),
                5 => Some(50),
                3 => Some(20),
                1 => Some(5),
                0 if full => Some(100),
                0 if value[1] & 0x03 == 0 => Some(0),
                _ => None,
            };
            Ok((percent, charging))
        }
        _ => bail!("not a battery register: 0x{register:02X}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid::protocol::SHORT_REPORT_ID;

    #[test]
    fn battery_register_levels_and_charging() {
        let parse = |register, value| parse_battery_register(register, value).unwrap();
        // 0x0D: percentage, charging when the high nibble of byte 2 is 0x5.
        assert_eq!(parse(REG_BATTERY_CHARGE, [64, 0, 0x30]), (Some(64), false));
        assert_eq!(parse(REG_BATTERY_CHARGE, [80, 0, 0x50]), (Some(80), true));
        // 0x07: coarse levels.
        assert_eq!(parse(REG_BATTERY_STATUS, [7, 0x00, 0]), (Some(90), false));
        assert_eq!(parse(REG_BATTERY_STATUS, [3, 0x21, 0]), (Some(20), true));
        assert_eq!(parse(REG_BATTERY_STATUS, [7, 0x22, 0]), (Some(90), false));
        // Level 0: empty, unless charging bits say "charging, level unknown".
        assert_eq!(parse(REG_BATTERY_STATUS, [0, 0x00, 0]), (Some(0), false));
        assert_eq!(parse(REG_BATTERY_STATUS, [0, 0x21, 0]), (None, true));
        // A level code we do not know is unknown, not empty.
        assert_eq!(parse(REG_BATTERY_STATUS, [4, 0x00, 0]), (None, false));
        assert!(parse_battery_register(0x02, [0, 0, 0]).is_err());
    }

    #[test]
    fn register_answers_match_slot_sub_id_and_register() {
        assert_eq!(
            device_register_read(4, REG_BATTERY_STATUS),
            [SHORT_REPORT_ID, 4, 0x81, 0x07, 0, 0, 0]
        );
        let get = SUB_GET_REGISTER;
        let reply = [SHORT_REPORT_ID, 4, 0x81, 0x07, 5, 0, 0];
        assert_eq!(
            match_register_answer(4, get, REG_BATTERY_STATUS, &reply),
            RegisterAnswer::Value([5, 0, 0])
        );
        assert_eq!(
            match_register_answer(3, get, REG_BATTERY_STATUS, &reply),
            RegisterAnswer::Unrelated
        );
        assert_eq!(
            match_register_answer(4, get, REG_BATTERY_CHARGE, &reply),
            RegisterAnswer::Unrelated
        );
        // A get reply is not the acknowledgement of a set.
        assert_eq!(
            match_register_answer(4, SUB_SET_REGISTER, REG_BATTERY_STATUS, &reply),
            RegisterAnswer::Unrelated
        );
        // ERR_INVALID_ADDRESS (0x02) for a register the device lacks.
        let error = [SHORT_REPORT_ID, 4, 0x8F, 0x81, 0x0D, 0x02, 0];
        assert_eq!(
            match_register_answer(4, get, REG_BATTERY_CHARGE, &error),
            RegisterAnswer::Rejected(0x02)
        );
        // A connection notification for another slot is not ours.
        let conn = [SHORT_REPORT_ID, 1, 0x41, 0x04, 0x02, 0x10, 0x20];
        assert_eq!(
            match_register_answer(4, get, REG_BATTERY_STATUS, &conn),
            RegisterAnswer::Unrelated
        );
    }

    #[test]
    fn receiver_requests_ignore_device_traffic() {
        // The receiver's own count reply (captured on C547) matches...
        let count = [SHORT_REPORT_ID, 0xFF, 0x81, 0x02, 0x00, 0x01, 0x00];
        assert_eq!(
            match_register_answer(
                RECEIVER_INDEX,
                SUB_GET_REGISTER,
                REG_CONNECTION_STATE,
                &count
            ),
            RegisterAnswer::Value([0x00, 0x01, 0x00])
        );
        // ...but a device's late register reply or error does not.
        let device_reply = [SHORT_REPORT_ID, 4, 0x81, 0x07, 0x05, 0x00, 0x00];
        let device_error = [SHORT_REPORT_ID, 4, 0x8F, 0x81, 0x02, 0x09, 0x00];
        for report in [device_reply, device_error] {
            assert_eq!(
                match_register_answer(
                    RECEIVER_INDEX,
                    SUB_GET_REGISTER,
                    REG_CONNECTION_STATE,
                    &report
                ),
                RegisterAnswer::Unrelated
            );
        }
    }

    #[test]
    fn device_battery_notification_bit_keeps_other_flags() {
        assert_eq!(
            with_battery_notifications([0x00, 0x00, 0x00]),
            Some([0x10, 0x00, 0x00])
        );
        // UI flag (0x02) set by someone else stays on.
        assert_eq!(
            with_battery_notifications([0x02, 0x01, 0x00]),
            Some([0x12, 0x01, 0x00])
        );
        assert_eq!(with_battery_notifications([0x10, 0x00, 0x00]), None);
        assert_eq!(
            register_frame(4, SUB_SET_REGISTER, REG_DEVICE_NOTIFICATIONS, [0x10, 0, 0]),
            [SHORT_REPORT_ID, 4, 0x80, 0x00, 0x10, 0, 0]
        );
    }

    #[test]
    fn hidpp20_request_errors_from_the_short_collection() {
        // Captured on C547 via --diag: a 2.0 ping (feature 0, byte 0x1A) to an
        // empty slot is answered on the short collection.
        let empty = [SHORT_REPORT_ID, 2, 0x8F, 0x00, 0x1A, 0x08, 0x00];
        assert_eq!(
            hidpp20_request_error(2, 0x00, 0x1A, &empty),
            Some(ERR_UNKNOWN_DEVICE)
        );
        assert_eq!(hidpp20_request_error(3, 0x00, 0x1A, &empty), None);
        assert_eq!(hidpp20_request_error(2, 0x00, 0x0A, &empty), None);
        // A register error is not an answer to a 2.0 request.
        let register_error = [SHORT_REPORT_ID, 1, 0x8F, 0x81, 0x0D, 0x01, 0x00];
        assert_eq!(hidpp20_request_error(1, 0x00, 0x1A, &register_error), None);
    }

    #[test]
    fn solaar_battery_vectors_decode_the_same() {
        // Solaar's tests/logitech_receiver/test_hidpp10.py `test_hidpp10_get_battery`
        // vectors. Solaar's FULL state is "not charging" here.
        let charge = [
            ([0x55, 0x00, 0x30], (Some(0x55), false)),
            ([0x44, 0x00, 0x50], (Some(0x44), true)),
            ([0x60, 0x00, 0x90], (Some(0x60), false)),
            ([0x22, 0x00, 0x00], (Some(0x22), false)),
        ];
        for (value, expected) in charge {
            assert_eq!(
                parse_battery_register(REG_BATTERY_CHARGE, value).unwrap(),
                expected
            );
        }
        let status = [
            ([0x07, 0x22, 0x00], (Some(90), false)),
            ([0x05, 0x21, 0x00], (Some(50), true)),
            ([0x03, 0x22, 0x00], (Some(20), false)),
            ([0x01, 0x01, 0x00], (Some(5), false)),
            // Solaar EMPTY.
            ([0x00, 0x00, 0x00], (Some(0), false)),
            // Solaar "full, no level": a full battery.
            ([0x00, 0x22, 0x00], (Some(100), false)),
        ];
        for (value, expected) in status {
            assert_eq!(
                parse_battery_register(REG_BATTERY_STATUS, value).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn battery_notifications_are_device_register_pushes() {
        assert!(is_battery_notification(&[
            SHORT_REPORT_ID,
            4,
            0x07,
            5,
            0,
            0,
            0
        ]));
        assert!(is_battery_notification(&[
            SHORT_REPORT_ID,
            2,
            0x0D,
            50,
            0,
            0x30,
            0
        ]));
        assert!(!is_battery_notification(&[
            SHORT_REPORT_ID,
            4,
            0x41,
            0x04,
            0x02,
            0x10,
            0x20
        ]));
        // Receiver-addressed traffic is never a device battery push.
        assert!(!is_battery_notification(&[
            SHORT_REPORT_ID,
            0xFF,
            0x07,
            0,
            0,
            0,
            0
        ]));
    }

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

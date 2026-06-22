/// HID++ 2.0 short report: 7 bytes (report id 0x10 prepended by hidapi write).
///
/// Wire layout (indices into the 7-byte payload, after report-id is stripped):
///   [0] = 0x10 (short report id, written by us as first byte of write buffer)
///   [1] = device_index
///   [2] = feature_index
///   [3] = (function_id << 4) | software_id
///   [4..6] = params (3 bytes)
pub const SHORT_REPORT_ID: u8 = 0x10;
/// HID++ long report id (20 bytes). Modern devices reply on this channel.
pub const LONG_REPORT_ID: u8 = 0x11;
pub const SW_ID: u8 = 0x0A;

/// Maximum times we retry a timed-out or busy response.
pub const MAX_RETRIES: usize = 5;
/// ms to wait for a response before retrying. Wireless round-trips through the
/// receiver can take a few hundred ms, so keep this comfortably above that.
pub const READ_TIMEOUT_MS: i32 = 500;

/// Budget for *presence* pings during slot discovery. This is only the fallback
/// path for receivers that don't push connection notifications — the primary
/// path detects devices from the receiver's `0x41` connection notification and
/// never pings. An idle device's round-trip through the receiver can exceed
/// 150ms, so we keep this moderate (cheaper than the full data budget, generous
/// enough to catch a linked-but-idle device) rather than aggressively short.
pub const PING_TIMEOUT_MS: i32 = 300;
pub const PING_RETRIES: usize = 2;

/// Classification of an incoming HID++ report, used to route unsolicited
/// notifications in the reader loop. Replies to our own pending requests are
/// matched separately (by exact feature index + software id); this is for
/// deciding what an *arriving* report means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportClass {
    /// Reply to one of our HID++ 2.0 feature requests (software id == [`SW_ID`]).
    Reply,
    /// Reply/ack to a HID++ 1.0 register request (sub-id 0x80 set / 0x81 get).
    RegisterReply { sub_id: u8 },
    /// Unsolicited HID++ 2.0 feature event (software id nibble == 0).
    FeatureEvent { device_index: u8, feature_index: u8 },
    /// HID++ 1.0 device-connection notification (sub-id 0x41).
    Connection { device_index: u8 },
    /// Other HID++ 1.0 receiver notification (sub-id 0x40..=0x7F).
    Notification { sub_id: u8 },
    /// Error reply (1.0 sub-id 0x8F or 2.0 feature index 0xFF).
    Error,
}

/// Normalize a raw HID read into the 7-byte HID++ header (report-id first).
///
/// hidapi on Windows may or may not prepend the report-id byte; a genuine HID++
/// report starts with 0x10 (short) or 0x11 (long). Mirrors the inline logic that
/// [`crate::hid::client`]'s `send_recv` has always used, factored out so the
/// reader loop and `--diag` classify reports the same way. Returns `None` for
/// reads too short to be a HID++ header.
pub fn normalize_report(buf: &[u8], n: usize) -> Option<[u8; 7]> {
    if n >= 8 && buf[0] != SHORT_REPORT_ID && buf[0] != LONG_REPORT_ID {
        // report-id was prepended — skip it
        Some(buf[1..8].try_into().unwrap())
    } else if n >= 7 {
        Some(buf[0..7].try_into().unwrap())
    } else {
        None
    }
}

/// Classify a normalized 7-byte report (`[0]` = report id, `[1]` = device index,
/// `[2]` = sub-id/feature index, `[3]` = register addr / `(function<<4)|swid`).
///
/// HID++ 1.0 (receiver) and 2.0 (device) share the wire frame, so we disambiguate
/// by `report[2]`: error codes (0x8F/0xFF) and register replies (>=0x80) first,
/// then receiver notifications (0x40..=0x7F, e.g. 0x41 connection), and finally
/// the 2.0 case where the software-id nibble of `report[3]` tells reply (ours)
/// from event (zero). Device feature indices for battery/root are small (<0x40),
/// so they never collide with the 1.0 sub-id ranges.
pub fn classify(report: &[u8; 7]) -> ReportClass {
    let id_byte = report[2];
    if id_byte == 0x8F || id_byte == 0xFF {
        return ReportClass::Error;
    }
    if id_byte >= 0x80 {
        return ReportClass::RegisterReply { sub_id: id_byte };
    }
    if id_byte >= 0x40 {
        return if id_byte == 0x41 {
            ReportClass::Connection {
                device_index: report[1],
            }
        } else {
            ReportClass::Notification { sub_id: id_byte }
        };
    }
    match report[3] & 0x0F {
        SW_ID => ReportClass::Reply,
        0x00 => ReportClass::FeatureEvent {
            device_index: report[1],
            feature_index: report[2],
        },
        // A reply addressed to some other software id — not ours to act on.
        other => ReportClass::Notification { sub_id: other },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShortMsg {
    pub device_index: u8,
    pub feature_index: u8,
    pub function_id: u8,
    pub params: [u8; 3],
}

impl ShortMsg {
    pub fn new(device_index: u8, feature_index: u8, function_id: u8, params: [u8; 3]) -> Self {
        Self {
            device_index,
            feature_index,
            function_id,
            params,
        }
    }

    /// Write the 6-byte payload (device_index, feature_index, function|sw_id,
    /// 3 params) that follows the report-id byte. Identical for short and long
    /// reports, so both encoders share it.
    fn write_payload(&self, out: &mut [u8]) {
        out[0] = self.device_index;
        out[1] = self.feature_index;
        out[2] = (self.function_id << 4) | SW_ID;
        out[3] = self.params[0];
        out[4] = self.params[1];
        out[5] = self.params[2];
    }

    /// Encode into a 7-byte write buffer (report_id first).
    pub fn encode(&self) -> [u8; 7] {
        let mut buf = [0u8; 7];
        buf[0] = SHORT_REPORT_ID;
        self.write_payload(&mut buf[1..7]);
        buf
    }

    /// Encode into a 20-byte HID++ long write buffer (report_id 0x11 first).
    ///
    /// The header and our 3 param bytes occupy the same offsets as the short
    /// report; the rest is zero-padded. Modern devices answer on the long
    /// channel, so requests go out as long.
    pub fn encode_long(&self) -> [u8; 20] {
        let mut buf = [0u8; 20];
        buf[0] = LONG_REPORT_ID;
        self.write_payload(&mut buf[1..7]);
        buf
    }

    /// Decode from a 7-byte read buffer.
    pub fn decode(buf: &[u8; 7]) -> Self {
        Self {
            device_index: buf[1],
            feature_index: buf[2],
            function_id: (buf[3] & 0xF0) >> 4,
            params: [buf[4], buf[5], buf[6]],
        }
    }

    pub fn sw_id(buf: &[u8; 7]) -> u8 {
        buf[3] & 0x0F
    }

    /// True when this is an error report rather than a normal reply.
    /// 0x8F = HID++ 1.0 receiver error (e.g. ERR_UNKNOWN_DEVICE for an empty
    /// slot); 0xFF = HID++ 2.0 device error. Neither is a valid feature index.
    pub fn is_error(buf: &[u8; 7]) -> bool {
        buf[2] == 0x8F || buf[2] == 0xFF
    }
}

/// Build a ping request for device_index (function 0x01 of feature 0x00 = IRoot).
/// The echo byte is returned by the device so we can verify it's alive.
pub fn ping(device_index: u8, echo: u8) -> ShortMsg {
    ShortMsg::new(device_index, 0x00, 0x01, [0x00, 0x00, echo])
}

/// Ask IRoot (feature 0x00) for the feature-table index of a given feature id.
pub fn get_feature(device_index: u8, feature_id: u16) -> ShortMsg {
    ShortMsg::new(
        device_index,
        0x00,
        0x00,
        [(feature_id >> 8) as u8, feature_id as u8, 0x00],
    )
}

/// Ask IFeatureSet for its count (function 0x00).
pub fn get_feature_count(device_index: u8, feature_set_idx: u8) -> ShortMsg {
    ShortMsg::new(device_index, feature_set_idx, 0x00, [0x00, 0x00, 0x00])
}

/// Ask IFeatureSet for the feature id at position i (function 0x01).
pub fn get_feature_id(device_index: u8, feature_set_idx: u8, i: u8) -> ShortMsg {
    ShortMsg::new(device_index, feature_set_idx, 0x01, [i, 0x00, 0x00])
}

/// Request battery status — feature 0x1000 / 0x1004, function 0x00.
pub fn get_battery_status(device_index: u8, feature_idx: u8) -> ShortMsg {
    ShortMsg::new(device_index, feature_idx, 0x00, [0x00, 0x00, 0x00])
}

/// Request battery level — feature 0x1004 uses function 0x01 ("getStatus").
pub fn get_unified_battery_status(device_index: u8, feature_idx: u8) -> ShortMsg {
    ShortMsg::new(device_index, feature_idx, 0x01, [0x00, 0x00, 0x00])
}

/// Request battery voltage — feature 0x1001, function 0x00.
pub fn get_battery_voltage(device_index: u8, feature_idx: u8) -> ShortMsg {
    ShortMsg::new(device_index, feature_idx, 0x00, [0x00, 0x00, 0x00])
}

/// Lookup table: index 0 = 100%, index 99 = 1%.
/// Voltage values (mV) taken from LGSTrayBattery / Solaar.
pub const VOLTAGE_LUT: [u16; 100] = [
    4186, 4156, 4143, 4133, 4122, 4113, 4103, 4094, 4086, 4075, 4067, 4059, 4051, 4043, 4035, 4027,
    4019, 4011, 4003, 3997, 3989, 3983, 3976, 3969, 3961, 3955, 3949, 3942, 3935, 3929, 3922, 3916,
    3909, 3902, 3896, 3890, 3883, 3877, 3870, 3865, 3859, 3853, 3848, 3842, 3837, 3833, 3828, 3824,
    3819, 3815, 3811, 3808, 3804, 3800, 3797, 3793, 3790, 3787, 3784, 3781, 3778, 3775, 3772, 3770,
    3767, 3764, 3762, 3759, 3757, 3754, 3751, 3748, 3744, 3741, 3737, 3734, 3730, 3726, 3724, 3720,
    3717, 3714, 3710, 3706, 3702, 3697, 3693, 3688, 3683, 3677, 3671, 3666, 3662, 3658, 3654, 3646,
    3633, 3612, 3579, 3537,
];

pub fn mv_to_percent(mv: u16) -> u8 {
    for (i, &threshold) in VOLTAGE_LUT.iter().enumerate() {
        if mv > threshold {
            return (100 - i) as u8;
        }
    }
    0
}

/// Decode charging status from feature 0x1000 / 0x1004 status byte.
pub fn is_charging_from_status(status: u8) -> bool {
    // 0 = discharging, 1 = recharging, 2 = charge in final stage, 3 = full, 4 = slow charge
    matches!(status, 1 | 2 | 4)
}

/// Decode charging status from feature 0x1001 flags byte.
pub fn is_charging_from_flags(flags: u8) -> bool {
    // bit 7: external power present; bits 0-2: charge status
    (flags & 0x80) != 0 && (flags & 0x07) != 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let msg = ShortMsg::new(1, 5, 0, [10, 20, 30]);
        let buf = msg.encode();
        let decoded = ShortMsg::decode(&buf);
        assert_eq!(decoded.device_index, 1);
        assert_eq!(decoded.feature_index, 5);
        assert_eq!(decoded.params, [10, 20, 30]);
    }

    #[test]
    fn mv_to_percent_boundaries() {
        assert_eq!(mv_to_percent(4200), 100);
        assert_eq!(mv_to_percent(3537), 0);
        assert_eq!(mv_to_percent(3800), 46);
    }

    #[test]
    fn sw_id_extracted() {
        let msg = ShortMsg::new(1, 0, 0, [0, 0, 42]);
        let buf = msg.encode();
        assert_eq!(ShortMsg::sw_id(&buf), SW_ID);
    }

    #[test]
    fn classify_distinguishes_report_kinds() {
        // Reply to one of our 2.0 feature requests: small feature idx, function 0,
        // software-id nibble == ours.
        let reply = [LONG_REPORT_ID, 0x01, 0x06, SW_ID, 0, 0, 0];
        assert_eq!(classify(&reply), ReportClass::Reply);

        // Unsolicited 2.0 battery event: small feature idx, swid nibble == 0.
        let event = [LONG_REPORT_ID, 0x01, 0x06, 0x00, 50, 0, 0];
        assert_eq!(
            classify(&event),
            ReportClass::FeatureEvent {
                device_index: 0x01,
                feature_index: 0x06
            }
        );

        // 1.0 device-connection notification.
        let conn = [SHORT_REPORT_ID, 0x02, 0x41, 0xA1, 0x00, 0x12, 0x34];
        assert_eq!(
            classify(&conn),
            ReportClass::Connection { device_index: 0x02 }
        );

        // 1.0 register get reply (sub-id 0x81).
        let reg = [SHORT_REPORT_ID, 0xFF, 0x81, 0x02, 0x01, 0, 0];
        assert_eq!(classify(&reg), ReportClass::RegisterReply { sub_id: 0x81 });

        // Error replies.
        let err10 = [SHORT_REPORT_ID, 0xFF, 0x8F, 0x00, 0x09, 0, 0];
        assert_eq!(classify(&err10), ReportClass::Error);
        let err20 = [LONG_REPORT_ID, 0x01, 0xFF, 0x06, 0x05, 0, 0];
        assert_eq!(classify(&err20), ReportClass::Error);
    }

    #[test]
    fn normalize_handles_prepended_report_id() {
        // No prepend: buffer already starts with the report id.
        let raw = [LONG_REPORT_ID, 0x01, 0x06, 0x0A, 1, 2, 3, 0];
        assert_eq!(
            normalize_report(&raw, 7),
            Some([LONG_REPORT_ID, 0x01, 0x06, 0x0A, 1, 2, 3])
        );

        // Prepended byte (not a report id) ahead of a genuine 0x11 header.
        let raw = [0x00, LONG_REPORT_ID, 0x01, 0x06, 0x0A, 1, 2, 3];
        assert_eq!(
            normalize_report(&raw, 8),
            Some([LONG_REPORT_ID, 0x01, 0x06, 0x0A, 1, 2, 3])
        );

        // Too short to be a header.
        assert_eq!(normalize_report(&[0x11, 0x01], 2), None);
    }
}

use crate::device_map;
use crate::hid::protocol::{
    LONG_REPORT_ID, MAX_RETRIES, READ_TIMEOUT_MS, SHORT_REPORT_ID, SW_ID, ShortMsg,
    get_battery_status, get_battery_voltage, get_feature, get_feature_count, get_feature_id,
    get_unified_battery_status, is_charging_from_flags, is_charging_from_status, mv_to_percent,
    ping,
};
use crate::hid::scanner::{open_receiver, scan_receivers};
use crate::model::{BatteryState, PollResult};
use anyhow::{Context, Result, bail};
use hidapi::{HidApi, HidDevice};
use std::collections::HashMap;
use std::thread;
use std::time::Duration;

const RETRY_DELAY: Duration = Duration::from_millis(100);

/// Per-device data that never changes while a device stays paired: which
/// battery feature to use (and its table index) and the display name. Feature
/// enumeration is many HID++ round-trips, so we do it once and reuse the result
/// on every subsequent poll — far less traffic, and it avoids re-running the
/// fragile multi-step enumeration that can transiently fail when the mouse is
/// half-asleep. `battery` is `None` for devices that expose no battery feature.
struct DeviceCache {
    battery: Option<(u16, u8)>,
    display_name: String,
}

/// Cache of [`DeviceCache`] keyed by `device_key` ("PID:index"). Held by the
/// poll worker for the lifetime of the app and passed into [`poll_devices`].
#[derive(Default)]
pub struct FeatureCache {
    devices: HashMap<String, DeviceCache>,
}

impl FeatureCache {
    pub fn new() -> Self {
        Self::default()
    }
}

pub fn poll_devices(api: &HidApi, cache: &mut FeatureCache) -> PollResult {
    let receivers = scan_receivers(api);
    let mut result = PollResult::default();

    if receivers.is_empty() {
        return result;
    }

    for receiver in &receivers {
        match open_receiver(api, receiver) {
            Ok(dev) => query_receiver(dev, receiver.pid, cache, &mut result),
            Err(err) => result.errors.push(format!("receiver {:04X}: {err}", receiver.pid)),
        }
    }

    result.sort_devices();
    result
}

fn query_receiver(
    mut dev: HidDevice,
    receiver_pid: u16,
    cache: &mut FeatureCache,
    result: &mut PollResult,
) {
    // Probe device indices 1–6 (Unifying supports up to 6 paired devices).
    for device_index in 1u8..=6 {
        let echo: u8 = 0x55 ^ device_index;
        match send_recv(&mut dev, ping(device_index, echo)) {
            Ok(buf) if buf[6] == echo => {
                // Device responded — query its battery.
                match query_device(&mut dev, device_index, receiver_pid, cache) {
                    Ok(Some(state)) => result.devices.push(state),
                    Ok(None) => {}
                    Err(err) => result.errors.push(format!(
                        "receiver {:04X} index {device_index}: {err}",
                        receiver_pid
                    )),
                }
            }
            _ => {}
        }
    }
}

fn query_device(
    dev: &mut HidDevice,
    device_index: u8,
    receiver_pid: u16,
    cache: &mut FeatureCache,
) -> Result<Option<BatteryState>> {
    let device_key = format!("{:04X}:{device_index}", receiver_pid);

    // Enumerate features (and read the static device name) only the first time
    // we see this device; reuse the cached result on every later poll.
    if !cache.devices.contains_key(&device_key) {
        let feature_map = build_feature_map(dev, device_index)
            .with_context(|| format!("feature enumeration failed for index {device_index}"))?;

        let battery = [0x1000u16, 0x1001, 0x1004]
            .iter()
            .find_map(|&id| feature_map.get(&id).map(|&idx| (id, idx)));

        let display_name = match battery {
            Some(_) => read_device_name(dev, device_index, &feature_map)
                .unwrap_or_else(|_| format!("Logitech Device (index {device_index})")),
            None => String::new(),
        };

        cache
            .devices
            .insert(device_key.clone(), DeviceCache { battery, display_name });
    }

    let entry = &cache.devices[&device_key];
    let Some((feature_id, feature_idx)) = entry.battery else {
        return Ok(None);
    };
    let display_name = entry.display_name.clone();

    let (battery_percent, is_charging) =
        read_battery(dev, device_index, feature_id, feature_idx)?;

    Ok(Some(BatteryState {
        device_key,
        display_name,
        pid: receiver_pid,
        device_index,
        battery_percent,
        is_charging,
    }))
}

/// Enumerate all HID++ 2.0 features for a device and return a map of feature_id → index.
fn build_feature_map(dev: &mut HidDevice, device_index: u8) -> Result<HashMap<u16, u8>> {
    // Step 1: get the index of IFeatureSet (0x0001) from IRoot (0x0000).
    let buf = send_recv(dev, get_feature(device_index, 0x0001))?;
    let feature_set_idx = buf[4];
    if feature_set_idx == 0 {
        bail!("IFeatureSet not found");
    }

    // Step 2: get feature count.
    let buf = send_recv(dev, get_feature_count(device_index, feature_set_idx))?;
    let count = buf[4];

    // Step 3: enumerate.
    let mut map = HashMap::new();
    map.insert(0x0001u16, feature_set_idx);

    for i in 1..=count {
        let buf = send_recv(dev, get_feature_id(device_index, feature_set_idx, i))?;
        let feature_id = ((buf[4] as u16) << 8) | buf[5] as u16;
        if feature_id != 0 {
            map.insert(feature_id, i);
        }
    }

    Ok(map)
}

/// Read the device name via feature 0x0005 (DEVICE_NAME_AND_TYPE).
/// Returns an error if the feature is absent, so the caller can fall back to a default.
fn read_device_name(
    dev: &mut HidDevice,
    device_index: u8,
    feature_map: &HashMap<u16, u8>,
) -> Result<String> {
    let &name_idx = feature_map
        .get(&0x0005)
        .context("DEVICE_NAME_AND_TYPE not found")?;

    // Get name length (function 0x00).
    let buf = send_recv(dev, ShortMsg::new(device_index, name_idx, 0x00, [0, 0, 0]))?;
    let name_len = buf[4] as usize;
    if name_len == 0 {
        bail!("empty name length");
    }

    // Read name in 3-byte chunks (function 0x01, param = byte offset).
    let mut name_bytes = Vec::with_capacity(name_len);
    while name_bytes.len() < name_len {
        let offset = name_bytes.len() as u8;
        let buf =
            send_recv(dev, ShortMsg::new(device_index, name_idx, 0x01, [offset, 0, 0]))?;
        // params are buf[4..7]
        for &b in &buf[4..7] {
            if name_bytes.len() < name_len {
                name_bytes.push(b);
            }
        }
    }

    let name = String::from_utf8_lossy(&name_bytes)
        .trim_end_matches('\0')
        .to_string();

    // If we have a prettier name in the device map, prefer it.
    Ok(device_map::display_name(&name))
}

fn read_battery(
    dev: &mut HidDevice,
    device_index: u8,
    feature_id: u16,
    feature_idx: u8,
) -> Result<(u8, bool)> {
    match feature_id {
        0x1000 => {
            let buf = send_recv(dev, get_battery_status(device_index, feature_idx))?;
            let percent = buf[4];
            let status = buf[6];
            Ok((percent, is_charging_from_status(status)))
        }
        0x1001 => {
            let buf = send_recv(dev, get_battery_voltage(device_index, feature_idx))?;
            let mv = ((buf[4] as u16) << 8) | buf[5] as u16;
            let flags = buf[6];
            Ok((mv_to_percent(mv), is_charging_from_flags(flags)))
        }
        0x1004 => {
            let buf = send_recv(dev, get_unified_battery_status(device_index, feature_idx))?;
            let percent = buf[4];
            let status = buf[6];
            Ok((percent, is_charging_from_status(status)))
        }
        _ => bail!("unsupported battery feature 0x{feature_id:04X}"),
    }
}

/// Send a HID++ short message and return the matching 7-byte response.
///
/// Discards non-matching responses (e.g. device notifications) and retries
/// on timeout. Returns an error after MAX_RETRIES attempts without a match.
fn send_recv(dev: &mut HidDevice, msg: ShortMsg) -> Result<[u8; 7]> {
    // Send requests as HID++ long reports (0x11). Wireless devices reply on the
    // long channel; a short request gets no reply on the long interface handle.
    let request = msg.encode_long();

    for attempt in 0..MAX_RETRIES {
        dev.write(&request).context("HID write failed")?;

        // Read responses until we get one that matches our request or time out.
        loop {
            let mut buf = [0u8; 21]; // 20-byte long report (+1 if report-id prepended)
            let n = dev
                .read_timeout(&mut buf, READ_TIMEOUT_MS)
                .context("HID read failed")?;

            if n == 0 {
                // Timeout — retry the whole request.
                if attempt + 1 < MAX_RETRIES {
                    thread::sleep(RETRY_DELAY);
                }
                break;
            }

            // Only the first 7 bytes carry the header + the params we read.
            // hidapi on Windows may or may not prepend the report-id byte; a
            // genuine HID++ report starts with 0x10 (short) or 0x11 (long).
            let response: [u8; 7] = if n >= 8 && buf[0] != SHORT_REPORT_ID && buf[0] != LONG_REPORT_ID {
                // report-id was prepended, skip it
                buf[1..8].try_into().unwrap()
            } else if n >= 7 {
                buf[0..7].try_into().unwrap()
            } else {
                continue;
            };

            if ShortMsg::is_error(&response) {
                bail!(
                    "HID++ error response for feature 0x{:02X}",
                    request[2]
                );
            }

            // Match on feature_index and SW_ID.
            if response[2] == request[2] && ShortMsg::sw_id(&response) == SW_ID {
                return Ok(response);
            }
            // Else: discard notification and keep reading.
        }
    }

    bail!(
        "no response after {MAX_RETRIES} attempts for feature 0x{:02X}",
        request[2]
    )
}

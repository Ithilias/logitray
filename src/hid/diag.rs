//! Hardware diagnostics for `--diag`. Prints to stdout; not used by the tray app.
//!
//! Covers the three suspected failure points when no devices are found:
//!   1. which VID 0x046D HID interfaces hidapi enumerates (usage page / interface)
//!   2. what `scan_receivers` actually matches
//!   3. whether each 0xFF00 interface answers a HID++ ping on indices 1..=6

use crate::hid::protocol::{self, ping, READ_TIMEOUT_MS, SHORT_REPORT_ID};
use crate::hid::receiver;
use crate::hid::scanner::{
    open_notifier, open_receiver, scan_receivers, ReceiverPath, LOGITECH_VID,
};
use hidapi::HidApi;

const HIDPP_USAGE_PAGE: u16 = 0xFF00;

pub fn run_diag() {
    let api = match HidApi::new() {
        Ok(api) => api,
        Err(err) => {
            println!("failed to init hidapi: {err}");
            return;
        }
    };

    println!("=== All VID 0x046D HID interfaces hidapi sees ===");
    let mut hidpp_paths = Vec::new();
    for info in api.device_list() {
        if info.vendor_id() != LOGITECH_VID {
            continue;
        }
        let product = info.product_string().unwrap_or("<none>");
        println!(
            "PID {:04X}  usage_page=0x{:04X} usage=0x{:04X} iface={:>3}  \"{}\"\n           path={}",
            info.product_id(),
            info.usage_page(),
            info.usage(),
            info.interface_number(),
            product,
            info.path().to_string_lossy(),
        );
        if info.usage_page() == HIDPP_USAGE_PAGE {
            hidpp_paths.push((info.product_id(), info.path().to_owned()));
        }
    }

    println!("\n=== scan_receivers() result ===");
    let receivers = scan_receivers(&api);
    if receivers.is_empty() {
        println!("(none matched VID 0x046D + usage_page 0x{HIDPP_USAGE_PAGE:04X})");
    } else {
        for r in &receivers {
            println!(
                "PID {:04X}\n   long  (0x0002) = {}\n   short (0x0001) = {}",
                r.pid,
                r.long_path.to_string_lossy(),
                r.short_path
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "<none>".to_string()),
            );
        }
    }

    // Collect the long-report interface (usage 0x0002) separately for the long ping probe.
    let long_paths: Vec<_> = api
        .device_list()
        .filter(|i| {
            i.vendor_id() == LOGITECH_VID
                && i.usage_page() == HIDPP_USAGE_PAGE
                && i.usage() == 0x0002
        })
        .map(|i| (i.product_id(), i.path().to_owned()))
        .collect();

    println!("\n=== SHORT ping (report 0x10) on each 0xFF00 interface, indices 1..=6 ===");
    if hidpp_paths.is_empty() {
        println!("(no 0xFF00 interface to probe)");
        return;
    }
    for (pid, path) in &hidpp_paths {
        println!("\n-- PID {:04X}  path={}", pid, path.to_string_lossy());
        let dev = match api.open_path(path.as_c_str()) {
            Ok(dev) => dev,
            Err(err) => {
                println!("   open failed: {err}");
                continue;
            }
        };
        println!("   opened OK");
        for device_index in 1u8..=6 {
            let echo: u8 = 0x55 ^ device_index;
            let req = ping(device_index, echo).encode();
            if let Err(err) = dev.write(&req) {
                println!("   index {device_index}: write failed: {err}");
                continue;
            }
            let mut buf = [0u8; 8];
            match dev.read_timeout(&mut buf, READ_TIMEOUT_MS) {
                Ok(0) => println!("   index {device_index}: timeout (no response)"),
                Ok(n) => {
                    let hex: Vec<String> = buf[..n].iter().map(|b| format!("{b:02X}")).collect();
                    let echo_ok = (n >= 7 && buf[6] == echo)
                        || (n == 8 && buf[0] != SHORT_REPORT_ID && buf[7] == echo);
                    println!(
                        "   index {device_index}: n={n} [{}]  echo({echo:02X}) match={echo_ok}",
                        hex.join(" ")
                    );
                }
                Err(err) => println!("   index {device_index}: read failed: {err}"),
            }
        }
    }

    println!(
        "\n=== LONG ping (report 0x11, 20 bytes) on usage=0x0002 interface, indices 1..=6 ==="
    );
    if long_paths.is_empty() {
        println!("(no usage=0x0002 long-report interface found)");
        return;
    }
    for (pid, path) in &long_paths {
        println!("\n-- PID {:04X}  path={}", pid, path.to_string_lossy());
        let dev = match api.open_path(path.as_c_str()) {
            Ok(dev) => dev,
            Err(err) => {
                println!("   open failed: {err}");
                continue;
            }
        };
        println!("   opened OK");
        for device_index in 1u8..=6 {
            let echo: u8 = 0x55 ^ device_index;
            // Long request: [0x11, device_index, feature=0x00, (func<<4)|swid=0x1A, 0,0,echo, pad..]
            let mut req = [0u8; 20];
            req[0] = 0x11;
            req[1] = device_index;
            req[2] = 0x00;
            req[3] = 0x1A;
            req[6] = echo;
            if let Err(err) = dev.write(&req) {
                println!("   index {device_index}: write failed: {err}");
                continue;
            }
            let mut buf = [0u8; 21];
            match dev.read_timeout(&mut buf, 1000) {
                Ok(0) => println!("   index {device_index}: timeout (no response)"),
                Ok(n) => {
                    let hex: Vec<String> = buf[..n].iter().map(|b| format!("{b:02X}")).collect();
                    println!("   index {device_index}: n={n} [{}]", hex.join(" "));
                }
                Err(err) => println!("   index {device_index}: read failed: {err}"),
            }
        }
    }

    notification_probe(&api, &receivers);
}

/// Probe the receiver's short collection (usage 0x0001), where HID++ 1.0
/// registers and the device-connection notification live. Enable notifications,
/// read the connected count, ask the receiver to re-announce, then dump and
/// classify reports for ~10s. A `Connection { .. }` here is the instant
/// device-arrival signal the tray uses; battery is then read on the long
/// collection. If the short collection is absent or silent, the app falls back
/// to the fast ping sweep + periodic safety re-read.
fn notification_probe(api: &HidApi, receivers: &[ReceiverPath]) {
    println!("\n=== Notification probe (short collection: enable + announce, listen ~10s) ===");
    for recv in receivers {
        println!("\n-- PID {:04X}", recv.pid);
        let dev = match open_notifier(api, recv) {
            Ok(dev) => dev,
            Err(err) => {
                println!("   {err}");
                continue;
            }
        };

        match receiver::enable_notifications(&dev) {
            Ok(()) => println!("   enable_notifications: OK"),
            Err(err) => println!("   enable_notifications: FAILED ({err})"),
        }
        match receiver::connected_count(&dev) {
            Ok(count) => println!("   connected_count: {count}"),
            Err(err) => println!("   connected_count: unavailable ({err})"),
        }
        match receiver::poke_announce(&dev) {
            Ok(()) => println!("   poke_announce: sent"),
            Err(err) => println!("   poke_announce: FAILED ({err})"),
        }

        // Also watch the long collection, where any unsolicited HID++ 2.0 battery
        // event would arrive. Toggle the charger during the listen window to see
        // whether the device pushes anything (and on which interface).
        let long = open_receiver(api, recv).ok();

        println!("   listening ~35s on SHORT + LONG — toggle the charger now...");
        let mut got_anything = false;
        for _ in 0..70 {
            for (label, handle) in [Some(("short", &dev)), long.as_ref().map(|l| ("long", l))]
                .into_iter()
                .flatten()
            {
                let mut buf = [0u8; 21];
                match handle.read_timeout(&mut buf, 250) {
                    Ok(0) => {}
                    Ok(n) => {
                        got_anything = true;
                        let hex: Vec<String> =
                            buf[..n].iter().map(|b| format!("{b:02X}")).collect();
                        let class = match protocol::normalize_report(&buf, n) {
                            Some(r) => {
                                let mut s = format!("{:?}", protocol::classify(&r));
                                if let Some(c) = receiver::parse_connection(&r) {
                                    s = format!("{s}  {c:?}");
                                }
                                s
                            }
                            None => "<runt>".to_string(),
                        };
                        println!("   [{label}] n={n} [{}]  -> {class}", hex.join(" "));
                    }
                    Err(err) => println!("   [{label}] read failed: {err}"),
                }
            }
        }
        if !got_anything {
            println!("   (no reports — receiver does not push notifications)");
        }
    }
}

//! Per-receiver device worker.
//!
//! Each Logitech receiver gets one thread that owns both of its vendor
//! collections: the **short** interface (HID++ 1.0 — receiver registers and the
//! device-connection notification) and the **long** interface (HID++ 2.0 —
//! battery data). The thread blocks reading the short interface for connection
//! notifications; when a device links up it reads that device's battery on the
//! long interface and emits an update. A relaxed safety timer re-reads battery
//! (it changes slowly) and re-asserts notifications as a resume guard.
//!
//! This replaces the old fixed-interval `poll_devices` sweep that blind-probed
//! all six slots every cycle — the cause of the multi-second cold-boot delay.
//! When a receiver exposes no short collection (no notifications), the worker
//! falls back to a fast bounded ping sweep plus the safety re-read.

use crate::config::{self, DeviceProfile, DeviceProfiles};
use crate::device_map;
use crate::hid::link::HidLink;
use crate::hid::protocol::{
    classify, get_battery_status, get_battery_voltage, get_feature, get_feature_count,
    get_feature_id, get_unified_battery_status, is_charging_from_flags, is_charging_from_status,
    mv_to_percent, normalize_report, ping, ReportClass, ShortMsg, MAX_RETRIES, PING_RETRIES,
    PING_TIMEOUT_MS, READ_TIMEOUT_MS, SW_ID,
};
use crate::hid::receiver::{self};
use crate::hid::scanner::{open_notifier, open_receiver, scan_receivers, ReceiverPath};
use crate::model::{BatteryState, PollResult};
use anyhow::{bail, Context, Result};
use hidapi::HidApi;
use std::collections::{BTreeSet, HashMap};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const RETRY_DELAY: Duration = Duration::from_millis(100);

/// How long the worker blocks reading the short interface before checking its
/// command channel / safety timer. Bounds command latency (e.g. Exit) and idle
/// wakeups; no USB traffic happens during the wait.
const SHORT_WAIT_MS: i32 = 1000;

/// Floor for the safety re-read interval. Battery changes slowly, so re-reading
/// more often than this just wastes USB round-trips.
const MIN_SAFETY_SECS: u64 = 15;

/// Highest device slot a receiver can pair (Unifying supports 6).
const MAX_SLOT: u8 = 6;

/// Idle devices may miss a complete ping retry budget while waking up. Require
/// consecutive failed discovery cycles before declaring a known device gone.
const MAX_MISSED_PING_CYCLES: u8 = 3;

/// Direct USB HID++ devices address themselves at 0xFF.
const DIRECT_DEVICE_INDEX: u8 = 0xFF;

/// Per-device data that never changes while a device stays paired: which battery
/// feature to use (and its table index) and the display name. Enumeration is
/// many HID++ round-trips, so we do it once and reuse it. `battery` is `None`
/// for devices that expose no battery feature.
struct DeviceCache {
    battery: Option<(u16, u8)>,
    display_name: String,
    wpid: u16,
}

/// Cache of [`DeviceCache`] keyed by `device_key` ("receiver-path:index"), held for the
/// worker's lifetime.
#[derive(Default)]
pub struct FeatureCache {
    devices: HashMap<String, DeviceCache>,
    missed_ping_cycles: HashMap<String, u8>,
}

impl FeatureCache {
    pub fn new() -> Self {
        Self::default()
    }
}

/// An update destined for the tray UI.
#[derive(Clone, Debug)]
pub enum DeviceEvent {
    /// A device arrived or its battery reading changed.
    Update(BatteryState),
    /// A device's link dropped; `device_key`.
    Gone(String),
    /// A whole receiver is no longer attached, so everything behind it is gone.
    ///
    /// Needed because `Gone` only ever comes from a link-down notification, and
    /// an unplugged receiver cannot send one: the worker either dies mid-read or
    /// (with no short collection) never notices at all. Without this the tray
    /// keeps every device the receiver was carrying.
    ReceiverGone(String),
}

/// Commands the tray sends to a receiver worker.
#[derive(Clone, Debug)]
pub enum WorkerCommand {
    /// Re-announce and re-read all known devices now (manual refresh).
    Refresh,
    /// Change the safety re-read interval (seconds).
    SetSafetyInterval(u64),
    /// Stop the worker thread.
    Exit,
}

fn device_key(receiver: &ReceiverPath, slot: u8) -> String {
    format!("{}:{slot}", receiver.key)
}

/// Spawn the worker thread for one receiver. `emit` is invoked (from the worker
/// thread) for every device update or departure; the tray forwards it to the
/// event loop. Returns the join handle.
pub fn spawn_receiver_worker<F>(
    receiver: ReceiverPath,
    safety_secs: u64,
    cmd_rx: mpsc::Receiver<WorkerCommand>,
    emit: F,
) -> thread::JoinHandle<()>
where
    F: Fn(DeviceEvent) + Send + 'static,
{
    thread::spawn(move || {
        if let Err(err) = run_worker(&receiver, safety_secs, &cmd_rx, &emit) {
            tracing::warn!("receiver {:04X} worker stopped: {err}", receiver.pid);
        }
    })
}

fn run_worker<F>(
    receiver: &ReceiverPath,
    safety_secs: u64,
    cmd_rx: &mpsc::Receiver<WorkerCommand>,
    emit: &F,
) -> Result<()>
where
    F: Fn(DeviceEvent),
{
    // hidapi handles aren't Send, so each worker owns its own HidApi and opens
    // both collections from the thread that uses them.
    let api = HidApi::new().context("failed to initialize hidapi")?;
    let mut long = open_receiver(&api, receiver).context("failed to open device interface")?;
    let short = open_notifier(&api, receiver)
        .ok()
        .map(|dev| Box::new(dev) as Box<dyn HidLink>);
    run_worker_on(
        &mut long,
        short,
        receiver,
        config::load_device_profiles(),
        safety_secs,
        cmd_rx,
        emit,
    )
}

/// The worker proper, on already-open collections (simulated ones in tests).
fn run_worker_on<F>(
    long: &mut dyn HidLink,
    short: Option<Box<dyn HidLink>>,
    receiver: &ReceiverPath,
    mut profiles: DeviceProfiles,
    safety_secs: u64,
    cmd_rx: &mpsc::Receiver<WorkerCommand>,
    emit: &F,
) -> Result<()>
where
    F: Fn(DeviceEvent),
{
    let short = short.as_deref();
    if detect_direct_usb(long, short) {
        return run_direct_worker(long, receiver, safety_secs, cmd_rx, emit);
    }
    if short.is_none() {
        tracing::info!(
            "receiver {:04X}: no short collection — using ping discovery + safety poll",
            receiver.pid
        );
    }

    let mut cache = FeatureCache::new();
    let mut known: BTreeSet<u8> = BTreeSet::new();
    let mut safety = Duration::from_secs(safety_secs.max(MIN_SAFETY_SECS));

    // Initial discovery. With notifications, enable them and ask the receiver to
    // announce its connected devices — the resulting 0x41 reports drive the read
    // loop below. Without, probe slots directly.
    if let Some(short) = short {
        // Notification setup was already attempted during transport detection.
        let _ = receiver::poke_announce(short);
    } else {
        discover_by_ping(long, receiver, &mut cache, &mut profiles, &mut known, emit);
    }

    let mut last_safety = Instant::now();
    loop {
        // Primary wait: a connection notification on the short interface, or a
        // timeout that lets us service commands and the safety timer.
        if let Some(short) = short {
            let mut buf = [0u8; 21];
            match short.read_timeout(&mut buf, SHORT_WAIT_MS) {
                Ok(0) => {}
                Ok(n) => {
                    if let Some(report) = normalize_report(&buf, n) {
                        if let Some(conn) = receiver::parse_connection(&report) {
                            handle_connection(
                                conn,
                                long,
                                receiver,
                                &mut cache,
                                &mut profiles,
                                &mut known,
                                emit,
                            );
                        }
                    }
                }
                Err(err) => bail!("short interface read failed: {err}"),
            }
        } else {
            // No notifications to wait on — idle until the next command/safety tick.
            thread::sleep(Duration::from_millis(SHORT_WAIT_MS as u64));
        }

        // Drain any unsolicited HID++ 2.0 battery events the device pushed on the
        // long interface (e.g. charging started/stopped) and reflect them right
        // away rather than waiting for the safety re-read.
        drain_long_events(long, receiver, &mut cache, &mut profiles, &known, emit);

        // Drain pending commands.
        loop {
            match cmd_rx.try_recv() {
                Ok(WorkerCommand::Refresh) => {
                    if let Some(short) = short {
                        let _ = receiver::poke_announce(short);
                    } else {
                        discover_by_ping(
                            long,
                            receiver,
                            &mut cache,
                            &mut profiles,
                            &mut known,
                            emit,
                        );
                    }
                    reread_known(long, receiver, &mut cache, &known, emit);
                }
                Ok(WorkerCommand::SetSafetyInterval(secs)) => {
                    safety = Duration::from_secs(secs.max(MIN_SAFETY_SECS));
                }
                Ok(WorkerCommand::Exit) => return Ok(()),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            }
        }

        // Safety timer: re-assert notifications (resume guard), re-discover on
        // the no-notification path, and re-read battery for known devices.
        if last_safety.elapsed() >= safety {
            if let Some(short) = short {
                let _ = receiver::enable_notifications(short);
                let _ = receiver::poke_announce(short);
            } else {
                discover_by_ping(long, receiver, &mut cache, &mut profiles, &mut known, emit);
            }
            reread_known(long, receiver, &mut cache, &known, emit);
            last_safety = Instant::now();
        }
    }
}

/// Use normal receiver setup first. A successful register request identifies
/// a receiver even when no wireless device is connected; do not pay USB ping
/// timeouts merely because its device list is empty.
fn detect_direct_usb(long: &mut dyn HidLink, short: Option<&dyn HidLink>) -> bool {
    direct_usb_fallback(
        || {
            short.is_some_and(|short| {
                receiver::enable_notifications(short)
                    .or_else(|_| receiver::connected_count(short).map(|_| ()))
                    .is_ok()
            })
        },
        || is_direct_device(long),
    )
}

fn direct_usb_fallback(
    receiver_setup: impl FnOnce() -> bool,
    usb_probe: impl FnOnce() -> bool,
) -> bool {
    !receiver_setup() && usb_probe()
}

// A receiver speaks HID++ 1.0 at 0xFF; a wired device answers the HID++ 2.0
// root ping there. Verify both the protocol version and echo, rather than
// inferring connection type from a product name or a guessed PID range.
fn is_direct_device(long: &mut dyn HidLink) -> bool {
    let echo = 0x55;
    send_recv_with(
        long,
        ping(DIRECT_DEVICE_INDEX, echo),
        PING_TIMEOUT_MS,
        PING_RETRIES,
    )
    .is_ok_and(|reply| direct_ping_reply(&reply, echo))
}

fn direct_ping_reply(reply: &[u8; 7], echo: u8) -> bool {
    reply[1] == DIRECT_DEVICE_INDEX && reply[4] >= 2 && reply[6] == echo
}

fn run_direct_worker<F>(
    long: &mut dyn HidLink,
    receiver: &ReceiverPath,
    safety_secs: u64,
    cmd_rx: &mpsc::Receiver<WorkerCommand>,
    emit: &F,
) -> Result<()>
where
    F: Fn(DeviceEvent),
{
    let mut cache = FeatureCache::new();
    let mut profiles = config::load_device_profiles();
    let mut known = BTreeSet::new();
    let mut safety = Duration::from_secs(safety_secs.max(MIN_SAFETY_SECS));
    let mut next_read = Instant::now();
    loop {
        if Instant::now() >= next_read {
            match read_device(
                long,
                DIRECT_DEVICE_INDEX,
                receiver,
                0,
                &mut cache,
                &mut profiles,
            ) {
                Ok(Some(state)) => {
                    known.insert(DIRECT_DEVICE_INDEX);
                    emit(DeviceEvent::Update(state));
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::debug!("USB device {:04X} battery read: {err}", receiver.pid);
                    if forget_slot(receiver, DIRECT_DEVICE_INDEX, &mut cache, &mut known) {
                        emit(DeviceEvent::Gone(device_key(receiver, DIRECT_DEVICE_INDEX)));
                    }
                }
            }
            next_read = Instant::now() + safety;
        }
        // Direct devices have no receiver connection announcements. Their
        // battery events arrive on the long interface and use index 0xFF.
        drain_long_events(long, receiver, &mut cache, &mut profiles, &known, emit);
        let wait = next_read
            .saturating_duration_since(Instant::now())
            .min(Duration::from_secs(1));
        match cmd_rx.recv_timeout(wait) {
            Ok(WorkerCommand::Refresh) => next_read = Instant::now(),
            Ok(WorkerCommand::SetSafetyInterval(secs)) => {
                safety = Duration::from_secs(secs.max(MIN_SAFETY_SECS));
                next_read = Instant::now();
            }
            Ok(WorkerCommand::Exit) | Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

/// React to a device-connection notification: read battery on link-up, emit a
/// departure on link-down.
fn handle_connection<F>(
    conn: receiver::ConnectionNotification,
    long: &mut dyn HidLink,
    receiver: &ReceiverPath,
    cache: &mut FeatureCache,
    profiles: &mut DeviceProfiles,
    known: &mut BTreeSet<u8>,
    emit: &F,
) where
    F: Fn(DeviceEvent),
{
    let pid = receiver.pid;
    let slot = conn.device_index;
    if !(1..=MAX_SLOT).contains(&slot) {
        return;
    }
    if conn.link_established {
        let key = device_key(receiver, slot);
        if invalidate_changed_device(cache, &key, conn.wpid) && known.remove(&slot) {
            emit(DeviceEvent::Gone(key));
        }
        match read_device(long, slot, receiver, conn.wpid, cache, profiles) {
            Ok(Some(state)) => {
                let newly = known.insert(slot);
                if newly {
                    tracing::info!(
                        "device connected: {} index {slot} — {}%",
                        state.display_name,
                        state.battery_percent
                    );
                }
                emit(DeviceEvent::Update(state));
            }
            Ok(None) => {} // device with no battery feature — ignore
            Err(err) => tracing::warn!("receiver {pid:04X} index {slot}: {err}"),
        }
    } else {
        if forget_slot(receiver, slot, cache, known) {
            tracing::info!("device disconnected: {pid:04X} index {slot}");
            emit(DeviceEvent::Gone(device_key(receiver, slot)));
        }
    }
}

/// Re-read battery for every currently-known device (safety timer / refresh).
fn reread_known<F>(
    long: &mut dyn HidLink,
    receiver: &ReceiverPath,
    cache: &mut FeatureCache,
    known: &BTreeSet<u8>,
    emit: &F,
) where
    F: Fn(DeviceEvent),
{
    let pid = receiver.pid;
    for &slot in known {
        match read_device(
            long,
            slot,
            receiver,
            0,
            cache,
            &mut DeviceProfiles::default(),
        ) {
            Ok(Some(state)) => emit(DeviceEvent::Update(state)),
            Ok(None) => {}
            Err(err) => tracing::debug!("receiver {pid:04X} index {slot} re-read: {err}"),
        }
    }
}

/// Drain unsolicited HID++ 2.0 events pending on the long interface. A device
/// pushes a battery [`ReportClass::FeatureEvent`] when its charge state changes
/// (e.g. the charger is plugged in/out); for a known device we re-read its
/// battery and emit the fresh reading. Non-blocking — returns as soon as no more
/// reports are queued.
fn drain_long_events<F>(
    long: &mut dyn HidLink,
    receiver: &ReceiverPath,
    cache: &mut FeatureCache,
    profiles: &mut DeviceProfiles,
    known: &BTreeSet<u8>,
    emit: &F,
) where
    F: Fn(DeviceEvent),
{
    let pid = receiver.pid;
    for _ in 0..32 {
        let mut buf = [0u8; 21];
        // Timeout 0 = non-blocking: return immediately if nothing is queued.
        match long.read_timeout(&mut buf, 0) {
            Ok(n) if n > 0 => {
                let Some(report) = normalize_report(&buf, n) else {
                    continue;
                };
                if let ReportClass::FeatureEvent { device_index, .. } = classify(&report) {
                    if known.contains(&device_index) {
                        match read_device(long, device_index, receiver, 0, cache, profiles) {
                            Ok(Some(state)) => {
                                tracing::info!(
                                    "battery event: {} — {}%{}",
                                    state.display_name,
                                    state.battery_percent,
                                    if state.is_charging { " (charging)" } else { "" }
                                );
                                emit(DeviceEvent::Update(state));
                            }
                            Ok(None) => {}
                            Err(err) => {
                                tracing::debug!(
                                    "receiver {pid:04X} index {device_index} event re-read: {err}"
                                )
                            }
                        }
                    }
                }
            }
            // Timeout (nothing queued) or read error: stop draining.
            _ => break,
        }
    }
}

/// Fallback discovery for receivers without a notification interface: probe
/// slots with the short ping budget and read whatever answers.
fn discover_by_ping<F>(
    long: &mut dyn HidLink,
    receiver: &ReceiverPath,
    cache: &mut FeatureCache,
    profiles: &mut DeviceProfiles,
    known: &mut BTreeSet<u8>,
    emit: &F,
) where
    F: Fn(DeviceEvent),
{
    let pid = receiver.pid;
    for slot in 1..=MAX_SLOT {
        let responded = ping_slot(long, slot);
        if record_ping_result(receiver, slot, responded, cache, known) {
            emit(DeviceEvent::Gone(device_key(receiver, slot)));
        }
        if !responded {
            continue;
        }
        match read_device(long, slot, receiver, 0, cache, profiles) {
            Ok(Some(state)) => {
                known.insert(slot);
                emit(DeviceEvent::Update(state));
            }
            Ok(None) => {}
            Err(err) => tracing::debug!("receiver {pid:04X} index {slot} discovery: {err}"),
        }
    }
}

/// Cheap presence check: a ping with the short discovery budget. Empty/asleep
/// slots cost ~PING_TIMEOUT_MS*PING_RETRIES instead of the full data budget.
fn ping_slot(dev: &mut dyn HidLink, slot: u8) -> bool {
    let echo: u8 = 0x55 ^ slot;
    matches!(
        send_recv_with(dev, ping(slot, echo), PING_TIMEOUT_MS, PING_RETRIES),
        Ok(buf) if buf[6] == echo
    )
}

/// Return true only on the cycle that removes an unresponsive known device.
/// A successful ping resets misses even if the subsequent battery read fails.
fn record_ping_result(
    receiver: &ReceiverPath,
    slot: u8,
    responded: bool,
    cache: &mut FeatureCache,
    known: &mut BTreeSet<u8>,
) -> bool {
    let key = device_key(receiver, slot);
    if responded {
        cache.missed_ping_cycles.remove(&key);
        return false;
    }
    if !known.contains(&slot) {
        return false;
    }
    let misses = cache.missed_ping_cycles.entry(key).or_default();
    *misses = misses.saturating_add(1);
    if *misses >= MAX_MISSED_PING_CYCLES {
        return forget_slot(receiver, slot, cache, known);
    }
    false
}

fn forget_slot(
    receiver: &ReceiverPath,
    slot: u8,
    cache: &mut FeatureCache,
    known: &mut BTreeSet<u8>,
) -> bool {
    let key = device_key(receiver, slot);
    cache.devices.remove(&key);
    cache.missed_ping_cycles.remove(&key);
    known.remove(&slot)
}

fn invalidate_changed_device(cache: &mut FeatureCache, key: &str, wpid: u16) -> bool {
    if wpid != 0
        && cache
            .devices
            .get(key)
            .is_some_and(|entry| entry.wpid != wpid)
    {
        cache.devices.remove(key);
        return true;
    }
    false
}

/// Read one device's battery, enumerating features (once) if needed. Prefers a
/// persisted [`DeviceProfile`] (keyed by WPID) to skip the fragile multi-round-
/// trip enumeration on a freshly-woken device; falls back to live enumeration
/// and rewrites the profile if the cached feature index no longer works.
fn read_device(
    dev: &mut dyn HidLink,
    slot: u8,
    receiver: &ReceiverPath,
    wpid: u16,
    cache: &mut FeatureCache,
    profiles: &mut DeviceProfiles,
) -> Result<Option<BatteryState>> {
    let key = device_key(receiver, slot);

    invalidate_changed_device(cache, &key, wpid);
    if !cache.devices.contains_key(&key) {
        let entry = resolve_device(dev, slot, wpid, profiles)?;
        cache.devices.insert(key.clone(), entry);
    }

    let entry = &cache.devices[&key];
    let Some((feature_id, feature_idx)) = entry.battery else {
        return Ok(None);
    };
    let display_name = entry.display_name.clone();
    let (battery_percent, is_charging) = read_battery(dev, slot, feature_id, feature_idx)?;

    Ok(Some(BatteryState {
        device_key: key,
        display_name,
        pid: receiver.pid,
        device_index: slot,
        battery_percent,
        is_charging,
    }))
}

/// Build a [`DeviceCache`] entry for a slot — via the persisted profile when the
/// WPID is known and the cached battery feature still answers, otherwise via
/// live feature enumeration (which then updates the persisted profile).
fn resolve_device(
    dev: &mut dyn HidLink,
    slot: u8,
    wpid: u16,
    profiles: &mut DeviceProfiles,
) -> Result<DeviceCache> {
    // Fast path: trust a persisted profile, validated by an actual battery read.
    if wpid != 0 {
        if let Some(profile) = profiles.get(wpid) {
            let id = profile.battery_feature_id;
            let idx = profile.battery_feature_index;
            if read_battery(dev, slot, id, idx).is_ok() {
                return Ok(DeviceCache {
                    battery: Some((id, idx)),
                    display_name: profile.name.clone(),
                    wpid,
                });
            }
            tracing::info!("WPID {wpid:04X} cached feature stale — re-enumerating");
        }
    }

    // Slow path: enumerate features (with a bounded retry, since a just-woken
    // device often flubs the first attempt) and pick a battery feature.
    let feature_map = build_feature_map_retry(dev, slot)?;
    let battery = [0x1000u16, 0x1001, 0x1004]
        .iter()
        .find_map(|&id| feature_map.get(&id).map(|&idx| (id, idx)));

    let display_name = match battery {
        Some(_) => read_device_name(dev, slot, &feature_map)
            .unwrap_or_else(|_| format!("Logitech Device (index {slot})")),
        None => String::new(),
    };

    // Persist what we learned so the next cold boot can skip all of the above.
    if wpid != 0 {
        if let Some((id, idx)) = battery {
            let profile = DeviceProfile {
                battery_feature_id: id,
                battery_feature_index: idx,
                name: display_name.clone(),
            };
            // Update this worker's own snapshot, then merge the one entry into
            // the file. Writing the whole snapshot back would drop whatever
            // another receiver's worker has learned since this thread started.
            // Tests drive this with simulated devices; their profiles stay in
            // memory rather than landing in the real devices.toml.
            if profiles.upsert(wpid, profile.clone()) && cfg!(not(test)) {
                if let Err(err) = config::save_device_profile(wpid, profile) {
                    tracing::debug!("failed saving device profile: {err}");
                }
            }
        }
    }

    Ok(DeviceCache {
        battery,
        display_name,
        wpid,
    })
}

/// One-shot synchronous read of every connected device across all receivers,
/// for `--once`. Discovers present devices from the receiver's connection
/// notifications (falling back to a ping sweep), then reads each one. No event
/// loop.
pub fn poll_once() -> PollResult {
    let mut result = PollResult::default();
    let api = match HidApi::new() {
        Ok(api) => api,
        Err(err) => {
            result
                .errors
                .push(format!("failed to initialize hidapi: {err}"));
            return result;
        }
    };

    let mut profiles = config::load_device_profiles();
    for receiver in scan_receivers(&api) {
        let mut long = match open_receiver(&api, &receiver) {
            Ok(dev) => dev,
            Err(err) => {
                result
                    .errors
                    .push(format!("receiver {:04X}: {err}", receiver.pid));
                continue;
            }
        };
        let short = open_notifier(&api, &receiver).ok();
        poll_receiver(
            &mut long,
            short.as_ref().map(|dev| dev as &dyn HidLink),
            &receiver,
            &mut profiles,
            &mut result,
        );
    }

    result.sort_devices();
    result
}

/// `--once` for one receiver, on already-open collections (simulated ones in
/// tests): discover the present slots and read each, into `result`.
fn poll_receiver(
    long: &mut dyn HidLink,
    short: Option<&dyn HidLink>,
    receiver: &ReceiverPath,
    profiles: &mut DeviceProfiles,
    result: &mut PollResult,
) {
    let mut cache = FeatureCache::new();
    for (slot, wpid) in discover_present_slots(long, short) {
        match read_device(long, slot, receiver, wpid, &mut cache, profiles) {
            Ok(Some(state)) => result.devices.push(state),
            Ok(None) => {}
            Err(err) => result
                .errors
                .push(format!("receiver {:04X} index {slot}: {err}", receiver.pid)),
        }
    }
}

/// Discover which slots currently hold a connected device, returning
/// `(slot, wpid)` pairs. Prefers the receiver's connection notifications (which
/// report link state reliably even for an idle device); falls back to a ping
/// sweep on receivers that expose no short collection.
fn discover_present_slots(long: &mut dyn HidLink, short: Option<&dyn HidLink>) -> Vec<(u8, u16)> {
    if detect_direct_usb(long, short) {
        return vec![(DIRECT_DEVICE_INDEX, 0)];
    }
    if let Some(short) = short {
        // How many devices to expect, so we can stop listening as soon as they've
        // all announced rather than always waiting out the deadline.
        let expected = receiver::connected_count(short).unwrap_or(0) as usize;
        let _ = receiver::poke_announce(short);

        let mut slots: Vec<(u8, u16)> = Vec::new();
        let deadline = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < deadline {
            let mut buf = [0u8; 21];
            match short.read_timeout(&mut buf, 300) {
                Ok(n) if n > 0 => {
                    if let Some(report) = normalize_report(&buf, n) {
                        if let Some(conn) = receiver::parse_connection(&report) {
                            if conn.link_established
                                && !slots.iter().any(|(s, _)| *s == conn.device_index)
                            {
                                slots.push((conn.device_index, conn.wpid));
                            }
                        }
                    }
                }
                _ => {}
            }
            if expected > 0 && slots.len() >= expected {
                break;
            }
        }
        if !slots.is_empty() {
            return slots;
        }
    }

    (1..=MAX_SLOT)
        .filter(|&slot| ping_slot(long, slot))
        .map(|slot| (slot, 0))
        .collect()
}

/// Enumerate features, retrying a couple of times — a device that just woke up
/// (e.g. right after a connection notification) often fails the first attempt.
fn build_feature_map_retry(dev: &mut dyn HidLink, device_index: u8) -> Result<HashMap<u16, u8>> {
    const ENUM_RETRIES: usize = 3;
    let mut last_err = None;
    for attempt in 0..ENUM_RETRIES {
        match build_feature_map(dev, device_index) {
            Ok(map) => return Ok(map),
            Err(err) => {
                last_err = Some(err);
                if attempt + 1 < ENUM_RETRIES {
                    thread::sleep(RETRY_DELAY);
                }
            }
        }
    }
    Err(last_err.unwrap())
        .with_context(|| format!("feature enumeration failed for index {device_index}"))
}

/// Enumerate all HID++ 2.0 features for a device and return a map of feature_id → index.
fn build_feature_map(dev: &mut dyn HidLink, device_index: u8) -> Result<HashMap<u16, u8>> {
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
    dev: &mut dyn HidLink,
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
        let buf = send_recv(
            dev,
            ShortMsg::new(device_index, name_idx, 0x01, [offset, 0, 0]),
        )?;
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
    dev: &mut dyn HidLink,
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

/// Send a HID++ short message and return the matching 7-byte response, using the
/// default data-read budget ([`MAX_RETRIES`]/[`READ_TIMEOUT_MS`]).
fn send_recv(dev: &mut dyn HidLink, msg: ShortMsg) -> Result<[u8; 7]> {
    send_recv_with(dev, msg, READ_TIMEOUT_MS, MAX_RETRIES)
}

/// Send a HID++ short message and return the matching 7-byte response.
///
/// Discards non-matching responses (e.g. device notifications) and retries on
/// timeout. `timeout_ms`/`retries` let callers pick a short budget for presence
/// pings vs. the generous budget for actual data reads. Returns an error after
/// `retries` attempts without a match.
fn send_recv_with(
    dev: &mut dyn HidLink,
    msg: ShortMsg,
    timeout_ms: i32,
    retries: usize,
) -> Result<[u8; 7]> {
    let request = msg.encode_long();
    for attempt in 0..retries {
        dev.write(&request).context("HID write failed")?;
        let response = wait_for_response(
            msg,
            Duration::from_millis(timeout_ms.max(1) as u64),
            |remaining| {
                let mut buf = [0u8; 21];
                let n = dev
                    .read_timeout(&mut buf, remaining)
                    .context("HID read failed")?;
                Ok((n != 0).then(|| normalize_report(&buf, n)).flatten())
            },
        )?;
        if let Some(response) = response {
            return Ok(response);
        }
        if attempt + 1 < retries {
            thread::sleep(RETRY_DELAY);
        }
    }
    bail!(
        "no response after {retries} attempts for feature 0x{:02X}",
        request[2]
    )
}

fn matches_reply(msg: ShortMsg, response: &[u8; 7]) -> bool {
    response[1] == msg.device_index
        && response[2] == msg.feature_index
        && response[3] == ((msg.function_id << 4) | SW_ID)
}

fn matches_error(msg: ShortMsg, response: &[u8; 7]) -> bool {
    ShortMsg::is_error(response)
        && response[1] == msg.device_index
        && response[3] == msg.feature_index
        && response[4] == ((msg.function_id << 4) | SW_ID)
}

fn wait_for_response(
    msg: ShortMsg,
    timeout: Duration,
    mut read: impl FnMut(i32) -> Result<Option<[u8; 7]>>,
) -> Result<Option<[u8; 7]>> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        let milliseconds = remaining.as_millis().max(1).min(i32::MAX as u128) as i32;
        let Some(response) = read(milliseconds)? else {
            continue;
        };
        if matches_error(msg, &response) {
            bail!(
                "HID++ error response for feature 0x{:02X}",
                msg.feature_index
            );
        }
        if matches_reply(msg, &response) {
            return Ok(Some(response));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disappearance_clears_only_the_departing_slot_and_its_cache() {
        let receiver = ReceiverPath {
            pid: 0xC547,
            key: "receiver-a".into(),
            long_path: std::ffi::CString::new("path").unwrap(),
            short_path: None,
        };
        let mut cache = FeatureCache::new();
        for slot in [1, 2] {
            cache.devices.insert(
                device_key(&receiver, slot),
                DeviceCache {
                    battery: Some((0x1000, 5)),
                    display_name: "old".into(),
                    wpid: 10,
                },
            );
        }
        let mut known = BTreeSet::from([1, 2]);
        assert!(forget_slot(&receiver, 1, &mut cache, &mut known));
        assert!(!forget_slot(&receiver, 1, &mut cache, &mut known));
        assert_eq!(known, BTreeSet::from([2]));
        assert_eq!(cache.devices.len(), 1);
        let key = device_key(&receiver, 2);
        invalidate_changed_device(&mut cache, &key, 10);
        assert!(cache.devices.contains_key(&key));
        invalidate_changed_device(&mut cache, &key, 11);
        assert!(!cache.devices.contains_key(&key));
    }
    #[test]
    fn idle_devices_require_three_consecutive_missed_discovery_cycles() {
        let receiver = ReceiverPath {
            pid: 0xC547,
            key: "receiver-a".into(),
            long_path: std::ffi::CString::new("path").unwrap(),
            short_path: None,
        };
        let mut cache = FeatureCache::new();
        let key = device_key(&receiver, 1);
        cache.devices.insert(
            key.clone(),
            DeviceCache {
                battery: Some((0x1000, 5)),
                display_name: "Mouse".into(),
                wpid: 10,
            },
        );
        let mut known = BTreeSet::from([1, 2]);
        for _ in 0..2 {
            assert!(!record_ping_result(
                &receiver, 1, false, &mut cache, &mut known
            ));
            assert!(known.contains(&1));
            assert!(cache.devices.contains_key(&key));
        }
        // An unrelated slot succeeding must not reset this device's misses.
        assert!(!record_ping_result(
            &receiver, 2, true, &mut cache, &mut known
        ));
        assert!(record_ping_result(
            &receiver, 1, false, &mut cache, &mut known
        ));
        assert_eq!(known, BTreeSet::from([2]));
        assert!(!cache.devices.contains_key(&key));
        assert!(!cache.missed_ping_cycles.contains_key(&key));
        assert!(!record_ping_result(
            &receiver, 1, false, &mut cache, &mut known
        ));
    }

    #[test]
    fn successful_ping_and_explicit_departure_reset_missed_cycles() {
        let receiver = ReceiverPath {
            pid: 0xC547,
            key: "receiver-a".into(),
            long_path: std::ffi::CString::new("path").unwrap(),
            short_path: None,
        };
        let mut cache = FeatureCache::new();
        let mut known = BTreeSet::from([1]);
        for responded in [false, false, true, false, false] {
            assert!(!record_ping_result(
                &receiver, 1, responded, &mut cache, &mut known
            ));
            assert!(known.contains(&1));
        }
        // A real disconnect is immediate, and a reconnect starts with no misses.
        assert!(forget_slot(&receiver, 1, &mut cache, &mut known));
        known.insert(1);
        for _ in 0..2 {
            assert!(!record_ping_result(
                &receiver, 1, false, &mut cache, &mut known
            ));
        }
        assert!(record_ping_result(
            &receiver, 1, false, &mut cache, &mut known
        ));
    }

    #[test]
    fn working_receiver_skips_usb_probe_even_with_zero_devices() {
        let connected_count = Ok::<u8, ()>(0);
        assert!(!direct_usb_fallback(
            || connected_count.is_ok(),
            || panic!("a responding receiver must not run the USB probe"),
        ));
    }

    #[test]
    fn usb_probe_runs_only_after_receiver_setup_fails() {
        use std::cell::Cell;
        let receiver_attempted = Cell::new(false);
        assert!(direct_usb_fallback(
            || {
                receiver_attempted.set(true);
                false
            },
            || {
                assert!(receiver_attempted.get());
                true
            },
        ));
        assert!(!direct_usb_fallback(|| false, || false));
    }

    #[test]
    fn direct_usb_detection_requires_device_address_protocol_and_echo() {
        assert!(direct_ping_reply(&[0x11, 0xFF, 0, 0x1A, 4, 2, 0x55], 0x55));
        assert!(!direct_ping_reply(&[0x11, 1, 0, 0x1A, 4, 2, 0x55], 0x55));
        assert!(!direct_ping_reply(&[0x11, 0xFF, 0, 0x1A, 1, 0, 0x55], 0x55));
        assert!(!direct_ping_reply(&[0x11, 0xFF, 0, 0x1A, 4, 2, 0x56], 0x55));
    }
    #[test]
    fn replies_must_match_device_and_function() {
        let msg = ShortMsg::new(1, 5, 1, [0; 3]);
        let reply = [0x11, 1, 5, 0x1A, 42, 0, 0];
        assert!(matches_reply(msg, &reply));
        assert!(!matches_reply(ShortMsg::new(2, 5, 1, [0; 3]), &reply));
        assert!(!matches_reply(ShortMsg::new(1, 5, 0, [0; 3]), &reply));
        assert!(matches_error(msg, &[0x11, 1, 0xFF, 5, 0x1A, 2, 0]));
        assert!(!matches_error(msg, &[0x11, 2, 0xFF, 5, 0x1A, 2, 0]));
    }
    #[test]
    fn continuous_unrelated_reports_cannot_extend_deadline() {
        let start = Instant::now();
        let result = wait_for_response(
            ShortMsg::new(1, 5, 1, [0; 3]),
            Duration::from_millis(20),
            |_| Ok(Some([0x11, 2, 5, 0x1A, 0, 0, 0])),
        )
        .unwrap();
        assert!(result.is_none());
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}

/// End-to-end tests against [`crate::hid::sim`]: the worker and `--once` paths
/// driving a simulated receiver.
#[cfg(test)]
mod sim_tests {
    use super::*;
    use crate::hid::sim::{Collection, SimDevice, SimReceiver};

    fn receiver() -> ReceiverPath {
        ReceiverPath {
            pid: 0xC52B,
            key: "sim".into(),
            long_path: std::ffi::CString::new("long").unwrap(),
            short_path: None,
        }
    }

    fn short_link(sim: &SimReceiver) -> Option<Box<dyn HidLink>> {
        Some(sim.link(Collection::Short))
    }

    /// `--once` against the simulator; devices sorted by slot.
    fn poll(
        sim: &SimReceiver,
        short: Option<Box<dyn HidLink>>,
        profiles: &mut DeviceProfiles,
    ) -> PollResult {
        let mut result = PollResult::default();
        let mut long = sim.link(Collection::Long);
        poll_receiver(
            &mut *long,
            short.as_deref(),
            &receiver(),
            profiles,
            &mut result,
        );
        result.devices.sort_by_key(|d| d.device_index);
        result
    }

    fn summary(result: &PollResult) -> Vec<(u8, &str, u8, bool)> {
        result
            .devices
            .iter()
            .map(|d| {
                let name = d.display_name.as_str();
                (d.device_index, name, d.battery_percent, d.is_charging)
            })
            .collect()
    }

    #[test]
    fn mouse_cold_start_enumerates_and_remembers_its_feature() {
        let sim = SimReceiver::new().with(1, SimDevice::g502(68));
        let mut profiles = DeviceProfiles::default();
        let result = poll(&sim, short_link(&sim), &mut profiles);

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(summary(&result), [(1, "G502 X PLUS", 68, false)]);
        let profile = profiles.get(0x4099).expect("G502 profile");
        assert_eq!(
            (profile.battery_feature_id, profile.battery_feature_index),
            (0x1004, 3)
        );
        assert_eq!(profile.name, "G502 X PLUS");
    }

    #[test]
    fn warm_start_reads_the_remembered_feature_directly() {
        let sim = SimReceiver::new().with(1, SimDevice::g502(68));
        let mut profiles = DeviceProfiles::default();
        poll(&sim, short_link(&sim), &mut profiles);
        let before = sim.writes().len();

        let result = poll(&sim, short_link(&sim), &mut profiles);
        assert_eq!(summary(&result), [(1, "G502 X PLUS", 68, false)]);
        // No enumeration: every request to the mouse goes to the battery feature.
        let new_writes = sim.writes().split_off(before);
        let to_mouse: Vec<_> = new_writes
            .iter()
            .filter(|(c, w)| *c == Collection::Long && w[1] == 1)
            .collect();
        assert!(!to_mouse.is_empty());
        assert!(to_mouse.iter().all(|(_, w)| w[2] == 3), "{to_mouse:?}");
    }

    #[test]
    fn mouse_missing_its_first_requests_is_still_read() {
        let sim = SimReceiver::new().with(1, SimDevice::g502(68).flaky(2));
        let result = poll(&sim, short_link(&sim), &mut DeviceProfiles::default());
        assert_eq!(
            summary(&result),
            [(1, "G502 X PLUS", 68, false)],
            "{:?}",
            result.errors
        );
    }

    #[test]
    fn two_mice_are_both_read() {
        let sim = SimReceiver::new()
            .with(1, SimDevice::g502(68))
            .with(3, SimDevice::mouse(0x4082, "PRO X SUPERLIGHT", 40));
        let result = poll(&sim, short_link(&sim), &mut DeviceProfiles::default());
        assert_eq!(
            summary(&result),
            [
                (1, "G502 X PLUS", 68, false),
                (3, "PRO X SUPERLIGHT", 40, false)
            ]
        );
    }

    #[test]
    fn without_a_short_collection_the_mouse_is_found_by_ping() {
        let sim = SimReceiver::new().with(1, SimDevice::g502(68));
        let result = poll(&sim, None, &mut DeviceProfiles::default());
        assert_eq!(summary(&result), [(1, "G502 X PLUS", 68, false)]);
        // Found by pinging the slots on the long collection.
        assert!(sim
            .writes_to(Collection::Long, 2)
            .iter()
            .any(|w| w[2] == 0x00 && w[3] >> 4 == 1));
    }

    /// A worker thread on the simulator; sends Exit and joins on drop.
    struct Worker {
        cmd: mpsc::Sender<WorkerCommand>,
        events: mpsc::Receiver<DeviceEvent>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl Worker {
        fn start(sim: &SimReceiver) -> Self {
            Self::start_with(sim, DeviceProfiles::default(), true, 60)
        }

        fn start_with(
            sim: &SimReceiver,
            profiles: DeviceProfiles,
            with_short: bool,
            safety_secs: u64,
        ) -> Self {
            let (cmd, cmd_rx) = mpsc::channel();
            let (event_tx, events) = mpsc::channel();
            let sim = sim.clone();
            let handle = thread::spawn(move || {
                let mut long = sim.link(Collection::Long);
                let short = if with_short { short_link(&sim) } else { None };
                let emit = move |event| {
                    let _ = event_tx.send(event);
                };
                run_worker_on(
                    &mut *long,
                    short,
                    &receiver(),
                    profiles,
                    safety_secs,
                    &cmd_rx,
                    &emit,
                )
                .unwrap();
            });
            Self {
                cmd,
                events,
                handle: Some(handle),
            }
        }

        fn next(&self) -> DeviceEvent {
            self.events
                .recv_timeout(Duration::from_secs(5))
                .expect("worker event")
        }

        fn update_for(&self, slot: u8) -> BatteryState {
            loop {
                if let DeviceEvent::Update(state) = self.next() {
                    if state.device_index == slot {
                        return state;
                    }
                }
            }
        }
    }

    impl Drop for Worker {
        fn drop(&mut self) {
            let _ = self.cmd.send(WorkerCommand::Exit);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    #[test]
    fn worker_reads_announced_mice() {
        let sim = SimReceiver::new()
            .with(1, SimDevice::g502(68))
            .with(2, SimDevice::g502(30));
        let worker = Worker::start(&sim);
        let mut seen = BTreeSet::new();
        while seen.len() < 2 {
            if let DeviceEvent::Update(state) = worker.next() {
                seen.insert((state.device_index, state.battery_percent));
            }
        }
        assert_eq!(seen, BTreeSet::from([(1, 68), (2, 30)]));
    }

    #[test]
    fn worker_refresh_shows_a_fresh_reading() {
        let sim = SimReceiver::new().with(1, SimDevice::g502(68));
        let worker = Worker::start(&sim);
        assert_eq!(worker.update_for(1).battery_percent, 68);

        sim.set_battery(1, 55);
        worker.cmd.send(WorkerCommand::Refresh).unwrap();
        while worker.update_for(1).battery_percent != 55 {}
    }

    #[test]
    fn worker_reports_a_mouse_link_down() {
        let sim = SimReceiver::new().with(1, SimDevice::g502(68));
        let worker = Worker::start(&sim);
        let state = worker.update_for(1);

        // 0x41 with the link-down bit (0x40) set.
        sim.push(Collection::Short, &[0x10, 1, 0x41, 0x04, 0x42, 0x99, 0x40]);
        loop {
            if let DeviceEvent::Gone(key) = worker.next() {
                assert_eq!(key, state.device_key);
                break;
            }
        }
    }

    #[test]
    fn worker_without_a_short_collection_finds_the_mouse() {
        let sim = SimReceiver::new().with(1, SimDevice::g502(68));
        let worker = Worker::start_with(&sim, DeviceProfiles::default(), false, 60);
        assert_eq!(worker.update_for(1).battery_percent, 68);
    }
}

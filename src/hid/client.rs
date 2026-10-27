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
//! Older HID++ 1.0 devices (e.g. the original K800) have no feature table. When
//! enumeration fails, the worker falls back to their battery registers, which
//! are read and pushed on the short interface instead.
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
use crate::hid::receiver::{self, RegisterAnswer, BATTERY_REGISTERS};
use crate::hid::scanner::{open_notifier, open_receiver, scan_receivers, ReceiverPath};
use crate::model::{BatteryState, PollResult};
use anyhow::{bail, Context, Result};
use hidapi::HidApi;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
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

/// How long to wait for a device's answer to a HID++ 1.0 register request or
/// to the protocol ping. These are sent once and never repeated: a 1.0 device
/// takes one request at a time and refuses a repeat as busy while the first is
/// pending. Solaar's `base.request` does the same with a 4 s window; half of
/// that keeps the worker responsive.
const DEVICE_REQUEST_TIMEOUT_MS: u64 = 2000;

/// The same for a register request to the receiver itself (Solaar: 0.9 s).
const RECEIVER_REQUEST_TIMEOUT_MS: u64 = 900;

/// Most reports [`ShortLink`] queues for the worker loop while a register read
/// is waiting. The short collection is quiet, so this is only a safety bound.
const SHORT_BACKLOG_CAP: usize = 32;

/// Where a device's battery reading comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BatterySource {
    /// HID++ 2.0 battery feature id and its feature-table index (long collection).
    Feature(u16, u8),
    /// HID++ 1.0 battery register (short collection), for devices without a
    /// feature table such as the original K800.
    Register(u8),
}

impl BatterySource {
    fn from_profile(profile: &DeviceProfile) -> Self {
        match profile.battery_register {
            Some(register) => Self::Register(register),
            None => Self::Feature(profile.battery_feature_id, profile.battery_feature_index),
        }
    }

    fn to_profile(self, name: String) -> DeviceProfile {
        let (battery_feature_id, battery_feature_index, battery_register) = match self {
            Self::Feature(id, idx) => (id, idx, None),
            Self::Register(register) => (0, 0, Some(register)),
        };
        DeviceProfile {
            battery_feature_id,
            battery_feature_index,
            battery_register,
            name,
        }
    }
}

/// Per-device data that never changes while a device stays paired: where to
/// read the battery and the display name. Enumeration is many HID++
/// round-trips, so we do it once and reuse it. `battery` is `None` for devices
/// that expose no battery feature.
struct DeviceCache {
    battery: Option<BatterySource>,
    display_name: String,
    wpid: u16,
    /// Last battery level read, for readings that carry none.
    last_percent: Option<u8>,
    /// A HID++ 1.0 device whose battery notifications still need switching on.
    notifications_pending: bool,
}

impl DeviceCache {
    fn new(battery: Option<BatterySource>, display_name: String, wpid: u16) -> Self {
        Self {
            battery,
            display_name,
            wpid,
            last_percent: None,
            notifications_pending: matches!(battery, Some(BatterySource::Register(_))),
        }
    }
}

/// The receiver's short collection. HID++ 1.0 device register reads share it
/// with the connection notifications the worker waits on, so a report that
/// arrives while a read waits for its reply is queued for the worker loop
/// instead of being dropped.
struct ShortLink {
    dev: Box<dyn HidLink>,
    backlog: VecDeque<[u8; 7]>,
}

impl ShortLink {
    fn new(dev: Box<dyn HidLink>) -> Self {
        Self {
            dev,
            backlog: VecDeque::new(),
        }
    }

    /// The next report: a queued one first, otherwise one read within
    /// `timeout_ms`. `None` on timeout or a read too short to be HID++.
    fn next_report(&mut self, timeout_ms: i32) -> Result<Option<[u8; 7]>> {
        if let Some(report) = self.backlog.pop_front() {
            return Ok(Some(report));
        }
        read_report(&*self.dev, timeout_ms).context("short interface read failed")
    }

    /// Queue a report for the worker loop.
    fn queue(&mut self, report: [u8; 7]) {
        push_bounded(&mut self.backlog, report);
    }

    /// Move reports already waiting on the collection into the backlog, so a
    /// late answer to an earlier request cannot pass for the answer to the next.
    fn drain_pending(&mut self) {
        for _ in 0..SHORT_BACKLOG_CAP {
            match read_report(&*self.dev, 0) {
                Ok(Some(report)) => self.queue(report),
                _ => break,
            }
        }
    }

    /// Send a HID++ 1.0 register request (`sub_id` 0x80 set / 0x81 get) to
    /// `slot` (a device, or the receiver at 0xFF) once and wait for its
    /// answer. A rejection is a [`receiver::RegisterRejected`] error.
    fn register_request(
        &mut self,
        slot: u8,
        sub_id: u8,
        register: u8,
        params: [u8; 3],
    ) -> Result<[u8; 3]> {
        let timeout = match slot {
            receiver::RECEIVER_INDEX => RECEIVER_REQUEST_TIMEOUT_MS,
            _ => DEVICE_REQUEST_TIMEOUT_MS,
        };
        self.drain_pending();
        let request = receiver::register_frame(slot, sub_id, register, params);
        self.dev.write(&request).context("HID write failed")?;
        let dev = &*self.dev;
        let answer = wait_for_register(
            slot,
            sub_id,
            register,
            Duration::from_millis(timeout),
            &mut self.backlog,
            |remaining| read_report(dev, remaining),
        )?;
        let Some(value) = answer else {
            bail!("no reply from index {slot} for register 0x{register:02X}");
        };
        tracing::debug!(
            "index {slot} register 0x{register:02X} (sub-id 0x{sub_id:02X}) = {value:02X?}"
        );
        Ok(value)
    }

    /// Read a HID++ 1.0 register from the device in `slot`.
    fn read_register(&mut self, slot: u8, register: u8) -> Result<[u8; 3]> {
        self.register_request(slot, receiver::SUB_GET_REGISTER, register, [0; 3])
    }

    /// Make the HID++ 1.0 device in `slot` push its battery register when the
    /// level changes, keeping its other notification flags. Waits for the
    /// write's acknowledgement: like Solaar, never send a 1.0 device a second
    /// request while one is outstanding.
    fn enable_battery_notifications(&mut self, slot: u8) -> Result<()> {
        let current = self.read_register(slot, receiver::REG_DEVICE_NOTIFICATIONS)?;
        if let Some(value) = receiver::with_battery_notifications(current) {
            self.register_request(
                slot,
                receiver::SUB_SET_REGISTER,
                receiver::REG_DEVICE_NOTIFICATIONS,
                value,
            )?;
        }
        Ok(())
    }

    /// [`receiver::enable_notifications`] through the backlog, for the worker:
    /// connection notifications arriving meanwhile are kept, not dropped.
    fn enable_receiver_notifications(&mut self) -> Result<()> {
        self.register_request(
            receiver::RECEIVER_INDEX,
            receiver::SUB_SET_REGISTER,
            receiver::REG_NOTIFICATIONS,
            receiver::notification_flags(),
        )?;
        Ok(())
    }
}

/// Append to a bounded backlog. A full backlog drops its oldest report, so the
/// latest connection state is what survives.
fn push_bounded(backlog: &mut VecDeque<[u8; 7]>, report: [u8; 7]) {
    if backlog.len() >= SHORT_BACKLOG_CAP {
        backlog.pop_front();
    }
    backlog.push_back(report);
}

/// One normalized report from `dev` within `timeout_ms`; `None` on timeout or a
/// read too short to be HID++.
fn read_report(dev: &dyn HidLink, timeout_ms: i32) -> Result<Option<[u8; 7]>> {
    let mut buf = [0u8; 21];
    let n = dev
        .read_timeout(&mut buf, timeout_ms)
        .context("HID read failed")?;
    Ok((n != 0).then(|| normalize_report(&buf, n)).flatten())
}

/// What a slot holds, as far as one HID++ 2.0 ping can tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotProtocol {
    /// Answered the ping: a HID++ 2.0 device.
    Hidpp20,
    /// Rejected the ping as an unknown sub-id: a HID++ 1.0 device.
    Hidpp10,
    /// The receiver says no device is connected there.
    Absent,
    /// Paired but not available right now (asleep, out of range, busy).
    Unavailable,
    /// No answer in time (e.g. still waking up), or an unexpected error.
    Unknown,
}

impl SlotProtocol {
    fn from_error(code: u8) -> Self {
        match code {
            receiver::ERR_INVALID_SUBID => Self::Hidpp10,
            receiver::ERR_UNKNOWN_DEVICE => Self::Absent,
            code if code == receiver::ERR_BUSY || receiver::is_unreachable(code) => {
                Self::Unavailable
            }
            _ => Self::Unknown,
        }
    }
}

/// Tell HID++ 1.0 from 2.0 with one ping, as Solaar's `base.ping` does: a short
/// report on the short collection. A 2.0 device answers on the long collection
/// (verified on a C547 + G502 via `--diag`); a 1.0 device rejects the unknown
/// sub-id with error 0x01, and the receiver answers for an empty or unreachable
/// slot. Errors are short reports and arrive on the short collection (an empty
/// slot's does, verified the same way); replies and errors are accepted on both
/// collections, since a device may answer a short request short. Sent once, see
/// [`DEVICE_REQUEST_TIMEOUT_MS`].
fn detect_protocol(long: &mut dyn HidLink, short: &mut ShortLink, slot: u8) -> SlotProtocol {
    let echo = 0x55 ^ slot;
    let msg = ping(slot, echo);
    let function_byte = (msg.function_id << 4) | SW_ID;
    let answer = |report: &[u8; 7]| {
        if matches_reply(msg, report) && report[6] == echo {
            return Some(SlotProtocol::Hidpp20);
        }
        receiver::hidpp20_request_error(slot, msg.feature_index, function_byte, report)
            .map(SlotProtocol::from_error)
    };
    short.drain_pending();
    if short.dev.write(&msg.encode()).is_err() {
        return SlotProtocol::Unknown;
    }
    let deadline = Instant::now() + Duration::from_millis(DEVICE_REQUEST_TIMEOUT_MS);
    while Instant::now() < deadline {
        match read_report(long, 10) {
            Ok(Some(report)) => {
                if let Some(protocol) = answer(&report) {
                    return protocol;
                }
            }
            Ok(None) => {}
            Err(_) => return SlotProtocol::Unknown,
        }
        match read_report(&*short.dev, 10) {
            Ok(Some(report)) => match answer(&report) {
                Some(protocol) => return protocol,
                None => short.queue(report),
            },
            Ok(None) => {}
            Err(_) => return SlotProtocol::Unknown,
        }
    }
    SlotProtocol::Unknown
}

/// Wait up to `timeout` for the answer to a register request, queueing every
/// unrelated report in `backlog`. `Ok(None)` on timeout, a
/// [`receiver::RegisterRejected`] error if the request was rejected.
fn wait_for_register(
    slot: u8,
    sub_id: u8,
    register: u8,
    timeout: Duration,
    backlog: &mut VecDeque<[u8; 7]>,
    mut read: impl FnMut(i32) -> Result<Option<[u8; 7]>>,
) -> Result<Option<[u8; 3]>> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        let milliseconds = remaining.as_millis().max(1).min(i32::MAX as u128) as i32;
        let Some(report) = read(milliseconds)? else {
            continue;
        };
        match receiver::match_register_answer(slot, sub_id, register, &report) {
            RegisterAnswer::Value(value) => return Ok(Some(value)),
            RegisterAnswer::Rejected(code) => {
                return Err(receiver::RegisterRejected { register, code })
                    .with_context(|| format!("index {slot}"));
            }
            RegisterAnswer::Unrelated => push_bounded(backlog, report),
        }
    }
}

/// True when `err` says the register itself is gone (the device no longer has
/// it, or no longer speaks HID++ 1.0), as opposed to a device that is asleep,
/// busy or slow to answer.
fn register_is_gone(err: &anyhow::Error) -> bool {
    rejection_code(err).is_some_and(|code| {
        matches!(
            code,
            receiver::ERR_INVALID_ADDRESS | receiver::ERR_INVALID_SUBID
        )
    })
}

/// True when `err` says the device cannot answer right now (busy, asleep, out
/// of range), so nothing can be learned about its registers from it.
fn device_is_unavailable(err: &anyhow::Error) -> bool {
    rejection_code(err)
        .is_some_and(|code| code == receiver::ERR_BUSY || receiver::is_unreachable(code))
}

/// A device that is paired but cannot answer right now (asleep, out of range,
/// busy). Its slot is resolved again when the device speaks up.
#[derive(Debug)]
struct Unavailable {
    slot: u8,
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "device at index {} is not available right now (asleep, out of range or busy)",
            self.slot
        )
    }
}

impl std::error::Error for Unavailable {}

fn rejection_code(err: &anyhow::Error) -> Option<u8> {
    err.downcast_ref::<receiver::RegisterRejected>()
        .map(|rejected| rejected.code)
}

/// Cache of [`DeviceCache`] keyed by `device_key` ("receiver-path:index"), held for the
/// worker's lifetime.
#[derive(Default)]
pub struct FeatureCache {
    devices: HashMap<String, DeviceCache>,
    missed_ping_cycles: HashMap<String, u8>,
    /// WPID of each slot the receiver announced as linked up.
    announced: HashMap<String, u16>,
    /// Announced slots whose device could not be resolved because it was
    /// [`Unavailable`] (asleep, busy).
    unavailable: HashSet<String>,
}

impl FeatureCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The WPID of a slot that was announced as linked up but could not be
    /// resolved because its device was unavailable (asleep, busy). Only those:
    /// a device that failed for any other reason would just fail again.
    fn unresolved_announcement(&self, key: &str) -> Option<u16> {
        self.announced
            .get(key)
            .copied()
            .filter(|_| self.unavailable.contains(key) && !self.devices.contains_key(key))
    }

    /// Whether the device under `key` is read through a HID++ 1.0 register.
    fn reads_register(&self, key: &str) -> bool {
        self.devices
            .get(key)
            .is_some_and(|entry| matches!(entry.battery, Some(BatterySource::Register(_))))
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
        .map(|dev| ShortLink::new(Box::new(dev)));
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
    mut short: Option<ShortLink>,
    receiver: &ReceiverPath,
    mut profiles: DeviceProfiles,
    safety_secs: u64,
    cmd_rx: &mpsc::Receiver<WorkerCommand>,
    emit: &F,
) -> Result<()>
where
    F: Fn(DeviceEvent),
{
    if detect_direct_usb(long, short.as_ref().map(|link| &*link.dev)) {
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
    if let Some(short) = &short {
        // Notification setup was already attempted during transport detection.
        let _ = receiver::poke_announce(&*short.dev);
    } else {
        discover_by_ping(long, receiver, &mut cache, &mut profiles, &mut known, emit);
    }

    let mut last_safety = Instant::now();
    loop {
        // Primary wait: a connection notification on the short interface, or a
        // timeout that lets us service commands and the safety timer.
        if let Some(short) = &mut short {
            if let Some(report) = short.next_report(SHORT_WAIT_MS)? {
                if let Some(conn) = receiver::parse_connection(&report) {
                    handle_connection(
                        conn,
                        long,
                        Some(short),
                        receiver,
                        &mut cache,
                        &mut profiles,
                        &mut known,
                        emit,
                    );
                } else if receiver::is_battery_notification(&report) {
                    let slot = report[1];
                    let key = device_key(receiver, slot);
                    if cache.reads_register(&key) {
                        // A HID++ 1.0 device pushed a battery change. (A 2.0
                        // device's short event can look the same; it is not ours.)
                        reread_after_event(
                            slot,
                            long,
                            Some(short),
                            receiver,
                            &mut cache,
                            &mut profiles,
                            &known,
                            emit,
                        );
                    } else if let Some(wpid) = cache.unresolved_announcement(&key) {
                        // Announced while it could not be resolved (asleep at a
                        // cold start); now it speaks up, so resolve it.
                        let conn = receiver::ConnectionNotification {
                            device_index: slot,
                            link_established: true,
                            wpid,
                        };
                        handle_connection(
                            conn,
                            long,
                            Some(short),
                            receiver,
                            &mut cache,
                            &mut profiles,
                            &mut known,
                            emit,
                        );
                    }
                }
            }
        } else {
            // No notifications to wait on — idle until the next command/safety tick.
            thread::sleep(Duration::from_millis(SHORT_WAIT_MS as u64));
        }

        // Drain any unsolicited HID++ 2.0 battery events the device pushed on the
        // long interface (e.g. charging started/stopped) and reflect them right
        // away rather than waiting for the safety re-read.
        drain_long_events(
            long,
            short.as_mut(),
            receiver,
            &mut cache,
            &mut profiles,
            &known,
            emit,
        );

        // Drain pending commands.
        loop {
            match cmd_rx.try_recv() {
                Ok(WorkerCommand::Refresh) => {
                    if let Some(short) = &short {
                        let _ = receiver::poke_announce(&*short.dev);
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
                    reread_known(long, short.as_mut(), receiver, &mut cache, &known, emit);
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
            if let Some(short) = &mut short {
                let _ = short.enable_receiver_notifications();
                let _ = receiver::poke_announce(&*short.dev);
            } else {
                discover_by_ping(long, receiver, &mut cache, &mut profiles, &mut known, emit);
            }
            reread_known(long, short.as_mut(), receiver, &mut cache, &known, emit);
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
                None,
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
        drain_long_events(
            long,
            None,
            receiver,
            &mut cache,
            &mut profiles,
            &known,
            emit,
        );
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
#[allow(clippy::too_many_arguments)]
fn handle_connection<F>(
    conn: receiver::ConnectionNotification,
    long: &mut dyn HidLink,
    short: Option<&mut ShortLink>,
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
            emit(DeviceEvent::Gone(key.clone()));
        }
        cache.announced.insert(key.clone(), conn.wpid);
        cache.unavailable.remove(&key);
        match read_device(long, short, slot, receiver, conn.wpid, cache, profiles) {
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
            Err(err) => {
                // Resolved but not readable yet (e.g. charging and no level
                // reported): keep it known so its pushes and the safety
                // re-read reach it.
                if cache.devices.contains_key(&key) {
                    known.insert(slot);
                }
                if err.downcast_ref::<Unavailable>().is_some() {
                    cache.unavailable.insert(key);
                }
                tracing::warn!("receiver {pid:04X} index {slot}: {err}");
            }
        }
    } else {
        if forget_slot(receiver, slot, cache, known) {
            tracing::info!("device disconnected: {pid:04X} index {slot}");
            emit(DeviceEvent::Gone(device_key(receiver, slot)));
        }
    }
}

/// A known device pushed a battery event (a HID++ 2.0 feature event, or a
/// HID++ 1.0 battery notification): re-read it and emit the fresh reading.
/// Re-reading rather than decoding the push also covers a 1.0 push that carries
/// only a charging change and no level.
#[allow(clippy::too_many_arguments)]
fn reread_after_event<F>(
    slot: u8,
    long: &mut dyn HidLink,
    short: Option<&mut ShortLink>,
    receiver: &ReceiverPath,
    cache: &mut FeatureCache,
    profiles: &mut DeviceProfiles,
    known: &BTreeSet<u8>,
    emit: &F,
) where
    F: Fn(DeviceEvent),
{
    if !known.contains(&slot) {
        return;
    }
    match read_device(long, short, slot, receiver, 0, cache, profiles) {
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
        Err(err) => tracing::debug!(
            "receiver {:04X} index {slot} event re-read: {err}",
            receiver.pid
        ),
    }
}

/// Re-read battery for every currently-known device (safety timer / refresh).
fn reread_known<F>(
    long: &mut dyn HidLink,
    mut short: Option<&mut ShortLink>,
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
            short.as_deref_mut(),
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
    mut short: Option<&mut ShortLink>,
    receiver: &ReceiverPath,
    cache: &mut FeatureCache,
    profiles: &mut DeviceProfiles,
    known: &BTreeSet<u8>,
    emit: &F,
) where
    F: Fn(DeviceEvent),
{
    for _ in 0..32 {
        let mut buf = [0u8; 21];
        // Timeout 0 = non-blocking: return immediately if nothing is queued.
        match long.read_timeout(&mut buf, 0) {
            Ok(n) if n > 0 => {
                let Some(report) = normalize_report(&buf, n) else {
                    continue;
                };
                if let ReportClass::FeatureEvent { device_index, .. } = classify(&report) {
                    reread_after_event(
                        device_index,
                        long,
                        short.as_deref_mut(),
                        receiver,
                        cache,
                        profiles,
                        known,
                        emit,
                    );
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
        // No short collection on this path, so HID++ 1.0 devices stay unread.
        match read_device(long, None, slot, receiver, 0, cache, profiles) {
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
    cache.announced.remove(&key);
    cache.unavailable.remove(&key);
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
    mut short: Option<&mut ShortLink>,
    slot: u8,
    receiver: &ReceiverPath,
    wpid: u16,
    cache: &mut FeatureCache,
    profiles: &mut DeviceProfiles,
) -> Result<Option<BatteryState>> {
    let key = device_key(receiver, slot);

    invalidate_changed_device(cache, &key, wpid);
    if !cache.devices.contains_key(&key) {
        let entry = resolve_device(dev, short.as_deref_mut(), slot, wpid, profiles)?;
        cache.devices.insert(key.clone(), entry);
    }

    let Some(source) = cache.devices[&key].battery else {
        return Ok(None);
    };
    let (level, is_charging) = read_source(dev, short.as_deref_mut(), slot, source)?;
    let entry = cache.devices.get_mut(&key).expect("resolved above");
    // The device forgets this setting when it powers off, and it may have been
    // asleep when last asked, so retry after each good read until it took
    // (Solaar re-sends it on every reconnect). A good read is enough, level or
    // not: a docked device reporting none is exactly the one whose pushes we
    // need. A device without the register is not asked again.
    if entry.notifications_pending {
        if let Some(short) = short {
            match short.enable_battery_notifications(slot) {
                Ok(()) => entry.notifications_pending = false,
                Err(err) => {
                    entry.notifications_pending = !register_is_gone(&err);
                    tracing::debug!("index {slot}: battery notifications not enabled: {err}");
                }
            }
        }
    }
    // A charging HID++ 1.0 device may report no level; keep showing its last
    // one. Only while charging: a stale low level must not outlive the charge.
    let Some(battery_percent) = level.or(entry.last_percent.filter(|_| is_charging)) else {
        bail!("index {slot} reports no battery level");
    };
    entry.last_percent = Some(battery_percent);
    let display_name = entry.display_name.clone();

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
/// WPID is known and the cached battery source still answers, otherwise via
/// live feature enumeration (which then updates the persisted profile). A
/// device that has no HID++ 2.0 feature table gets one more chance as a HID++
/// 1.0 device with a battery register.
fn resolve_device(
    dev: &mut dyn HidLink,
    mut short: Option<&mut ShortLink>,
    slot: u8,
    wpid: u16,
    profiles: &mut DeviceProfiles,
) -> Result<DeviceCache> {
    // Fast path: trust a persisted profile, validated by an actual battery read.
    if wpid != 0 {
        if let Some(profile) = profiles.get(wpid) {
            let source = BatterySource::from_profile(profile);
            let cached = DeviceCache::new(Some(source), profile.name.clone(), wpid);
            match read_source(dev, short.as_deref_mut(), slot, source) {
                Ok(_) => return Ok(cached),
                // A register device that is asleep, busy or slow keeps its
                // profile: resolving again cannot do better, and would cost the
                // whole slow path on every announcement. Its own push, or the
                // next announcement, resolves it once it answers.
                Err(err)
                    if matches!(source, BatterySource::Register(_)) && !register_is_gone(&err) =>
                {
                    return Err(err.context(Unavailable { slot }));
                }
                Err(_) => tracing::info!("WPID {wpid:04X} cached feature stale — re-enumerating"),
            }
        }
    }

    // Slow path. A HID++ 1.0 device never answers feature enumeration (its
    // rejections go to the short collection), so tell the protocols apart first
    // instead of waiting out every enumeration timeout.
    let mut protocol = SlotProtocol::Unknown;
    if let Some(short) = short.as_deref_mut() {
        protocol = detect_protocol(dev, short, slot);
        match protocol {
            SlotProtocol::Hidpp10 => {
                return resolve_register_device(short, slot, wpid, profiles)?
                    .context("HID++ 1.0 device answers no battery register");
            }
            SlotProtocol::Absent => bail!("no device connected at index {slot}"),
            SlotProtocol::Unavailable => return Err(Unavailable { slot }.into()),
            SlotProtocol::Hidpp20 | SlotProtocol::Unknown => {}
        }
    }

    // Enumerate features (with a bounded retry, since a just-woken device often
    // flubs the first attempt) and pick a battery feature.
    let feature_map = match build_feature_map_retry(dev, slot) {
        Ok(map) => map,
        // Only when the ping went unanswered: a 1.0 device still gets its
        // register probe. A device that answered as 2.0 never does.
        Err(err) => match short {
            Some(short) if protocol == SlotProtocol::Unknown => {
                return resolve_register_device(short, slot, wpid, profiles)?.ok_or(err);
            }
            _ => return Err(err),
        },
    };
    let battery = [0x1000u16, 0x1001, 0x1004].iter().find_map(|&id| {
        feature_map
            .get(&id)
            .map(|&idx| BatterySource::Feature(id, idx))
    });

    let display_name = match battery {
        Some(_) => read_device_name(dev, slot, &feature_map)
            .unwrap_or_else(|_| format!("Logitech Device (index {slot})")),
        None => String::new(),
    };

    if let Some(battery) = battery {
        remember_profile(profiles, wpid, battery, &display_name);
    }

    Ok(DeviceCache::new(battery, display_name, wpid))
}

/// Resolve a HID++ 1.0 device through its battery registers: `Ok(None)` if it
/// answers none.
fn resolve_register_device(
    short: &mut ShortLink,
    slot: u8,
    wpid: u16,
    profiles: &mut DeviceProfiles,
) -> Result<Option<DeviceCache>> {
    let Some(register) = probe_battery_register(short, slot)? else {
        return Ok(None);
    };
    tracing::info!("index {slot}: HID++ 1.0 device, battery register 0x{register:02X}");
    let display_name = device_map::name_for_wpid(wpid)
        .map(str::to_string)
        .unwrap_or_else(|| format!("Logitech Device (index {slot})"));
    let battery = BatterySource::Register(register);
    remember_profile(profiles, wpid, battery, &display_name);
    // Battery notifications are switched on after the first successful read.
    Ok(Some(DeviceCache::new(Some(battery), display_name, wpid)))
}

/// Persist what we learned so the next cold boot can skip resolving it again.
fn remember_profile(
    profiles: &mut DeviceProfiles,
    wpid: u16,
    battery: BatterySource,
    display_name: &str,
) {
    if wpid == 0 {
        return;
    }
    let profile = battery.to_profile(display_name.to_string());
    // Update this worker's own snapshot, then merge the one entry into
    // the file. Writing the whole snapshot back would drop whatever
    // another receiver's worker has learned since this thread started.
    // Tests drive this with simulated devices; their profiles stay in memory
    // rather than landing in the real devices.toml.
    if profiles.upsert(wpid, profile.clone()) && cfg!(not(test)) {
        if let Err(err) = config::save_device_profile(wpid, profile) {
            tracing::debug!("failed saving device profile: {err}");
        }
    }
}

/// The first battery register a HID++ 1.0 device answers, probed in Solaar's
/// order. An answer is enough: a 0x07 value without a level (charging-only)
/// still identifies the register, and the next read gets the level.
///
/// The choice is remembered for good, so a device that cannot answer right now
/// (busy, asleep) ends the probe with an error rather than moving on and
/// settling on the coarser register; a rejected or unanswered register is
/// taken as absent, as Solaar does.
fn probe_battery_register(short: &mut ShortLink, slot: u8) -> Result<Option<u8>> {
    for register in BATTERY_REGISTERS {
        match short.read_register(slot, register) {
            Ok(_) => return Ok(Some(register)),
            Err(err) if device_is_unavailable(&err) => return Err(err),
            Err(err) => tracing::debug!("battery register probe: {err}"),
        }
    }
    Ok(None)
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
        let short = open_notifier(&api, &receiver)
            .ok()
            .map(|dev| ShortLink::new(Box::new(dev)));
        poll_receiver(&mut long, short, &receiver, &mut profiles, &mut result);
    }

    result.sort_devices();
    result
}

/// `--once` for one receiver, on already-open collections (simulated ones in
/// tests): discover the present slots and read each, into `result`.
fn poll_receiver(
    long: &mut dyn HidLink,
    mut short: Option<ShortLink>,
    receiver: &ReceiverPath,
    profiles: &mut DeviceProfiles,
    result: &mut PollResult,
) {
    let mut cache = FeatureCache::new();
    let slots = discover_present_slots(long, short.as_ref().map(|link| &*link.dev));
    for (slot, wpid) in slots {
        // A wired device has no HID++ 1.0 side; the worker passes none either.
        let short = match slot {
            DIRECT_DEVICE_INDEX => None,
            _ => short.as_mut(),
        };
        match read_device(long, short, slot, receiver, wpid, &mut cache, profiles) {
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

fn read_source(
    dev: &mut dyn HidLink,
    short: Option<&mut ShortLink>,
    device_index: u8,
    source: BatterySource,
) -> Result<(Option<u8>, bool)> {
    match source {
        BatterySource::Feature(id, idx) => {
            let (percent, charging) = read_battery(dev, device_index, id, idx)?;
            Ok((Some(percent), charging))
        }
        BatterySource::Register(register) => {
            let short = short.context("HID++ 1.0 battery needs the short collection")?;
            let value = short.read_register(device_index, register)?;
            receiver::parse_battery_register(register, value)
        }
    }
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
                    battery: Some(BatterySource::Feature(0x1000, 5)),
                    display_name: "old".into(),
                    wpid: 10,
                    last_percent: None,
                    notifications_pending: false,
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
                battery: Some(BatterySource::Feature(0x1000, 5)),
                display_name: "Mouse".into(),
                wpid: 10,
                last_percent: None,
                notifications_pending: false,
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
    fn register_read_keeps_notifications_that_arrive_meanwhile() {
        // Slot 1's connection notification lands while slot 4's register read
        // waits; the worker loop must still see it afterwards.
        let conn = [0x10, 1, 0x41, 0x04, 0x02, 0x6A, 0x40];
        let reply = [0x10, 4, 0x81, 0x07, 5, 0, 0];
        let mut reports = vec![conn, reply].into_iter();
        let mut backlog = VecDeque::new();
        let value = wait_for_register(4, 0x81, 0x07, Duration::from_secs(1), &mut backlog, |_| {
            Ok(reports.next())
        })
        .unwrap();
        assert_eq!(value, Some([5, 0, 0]));
        assert_eq!(backlog, VecDeque::from([conn]));

        // A rejection is an error, not a timeout, and leaves the backlog alone.
        let error = [0x10, 4, 0x8F, 0x81, 0x0D, 0x02, 0];
        let mut backlog = VecDeque::new();
        assert!(
            wait_for_register(4, 0x81, 0x0D, Duration::from_secs(1), &mut backlog, |_| Ok(
                Some(error)
            ))
            .is_err()
        );
        assert!(backlog.is_empty());

        // Endless unrelated traffic times out and stays bounded.
        let mut backlog = VecDeque::new();
        let value = wait_for_register(
            4,
            0x81,
            0x07,
            Duration::from_millis(20),
            &mut backlog,
            |_| Ok(Some(conn)),
        )
        .unwrap();
        assert!(value.is_none());
        assert_eq!(backlog.len(), SHORT_BACKLOG_CAP);
    }

    #[test]
    fn battery_source_roundtrips_through_profiles() {
        for source in [
            BatterySource::Feature(0x1004, 8),
            BatterySource::Register(0x07),
        ] {
            let profile = source.to_profile("Device".into());
            assert_eq!(BatterySource::from_profile(&profile), source);
        }
        assert_eq!(
            BatterySource::Register(0x07).to_profile("K800".into()),
            DeviceProfile {
                battery_feature_id: 0,
                battery_feature_index: 0,
                battery_register: Some(0x07),
                name: "K800".into(),
            }
        );
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
/// driving a simulated receiver with a HID++ 2.0 mouse and a HID++ 1.0 K800.
#[cfg(test)]
mod sim_tests {
    use super::*;
    use crate::hid::sim::{Collection, ErrorRoute, SimDevice, SimReceiver};

    const K800_NAME: &str = "Wireless Illuminated Keyboard K800";

    fn receiver() -> ReceiverPath {
        ReceiverPath {
            pid: 0xC52B,
            key: "sim".into(),
            long_path: std::ffi::CString::new("long").unwrap(),
            short_path: None,
        }
    }

    fn short_link(sim: &SimReceiver) -> Option<ShortLink> {
        Some(ShortLink::new(sim.link(Collection::Short)))
    }

    /// `--once` against the simulator; devices sorted by slot.
    fn poll(
        sim: &SimReceiver,
        short: Option<ShortLink>,
        profiles: &mut DeviceProfiles,
    ) -> PollResult {
        let mut result = PollResult::default();
        let mut long = sim.link(Collection::Long);
        poll_receiver(&mut *long, short, &receiver(), profiles, &mut result);
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

    fn register_reads(writes: &[Vec<u8>], register: u8) -> usize {
        writes
            .iter()
            .filter(|w| w[2] == 0x81 && w[3] == register)
            .count()
    }

    /// HID++ 1.0 register requests (sub-id 0x80 set / 0x81 get) among `writes`.
    fn register_requests(writes: &[Vec<u8>]) -> usize {
        writes
            .iter()
            .filter(|w| w[2] == 0x80 || w[2] == 0x81)
            .count()
    }

    fn hidpp10(registers: &[(u8, [u8; 3])], error_route: ErrorRoute) -> SimDevice {
        let mut registers: HashMap<u8, [u8; 3]> = registers.iter().copied().collect();
        registers.insert(0x00, [0, 0, 0]);
        SimDevice::Hidpp10 {
            wpid: 0x2010,
            registers,
            error_route,
        }
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

    #[test]
    fn k800_cold_start_reads_its_register_and_remembers_it() {
        let sim = SimReceiver::new()
            .with(1, SimDevice::g502(68))
            .with(4, SimDevice::k800([0x05, 0x21, 0x00]));
        let mut profiles = DeviceProfiles::default();
        let start = Instant::now();
        let result = poll(&sim, short_link(&sim), &mut profiles);

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(
            summary(&result),
            [(1, "G502 X PLUS", 68, false), (4, K800_NAME, 50, true)]
        );
        // The protocol ping went out short; nothing on the long collection.
        assert!(sim.writes_to(Collection::Long, 4).is_empty());
        assert!(start.elapsed() < Duration::from_secs(3));
        // 0x0D rejected, 0x07 used and remembered.
        let k800 = profiles.get(0x2010).expect("K800 profile");
        assert_eq!(k800.battery_register, Some(0x07));
        assert_eq!(k800.name, K800_NAME);
        // Battery notifications switched on in the device itself.
        assert_eq!(sim.register(4, 0x00), Some([0x10, 0x00, 0x00]));
        // The 2.0 mouse never sees a register read.
        assert_eq!(register_requests(&sim.writes_to(Collection::Short, 1)), 0);
    }

    #[test]
    fn battery_charge_register_wins_when_present() {
        let device = hidpp10(
            &[(0x0D, [0x55, 0x00, 0x30]), (0x07, [0x07, 0x00, 0x00])],
            ErrorRoute::Short,
        );
        let sim = SimReceiver::new().with(2, device);
        let mut profiles = DeviceProfiles::default();
        let result = poll(&sim, short_link(&sim), &mut profiles);

        assert_eq!(summary(&result), [(2, K800_NAME, 85, false)]);
        assert_eq!(profiles.get(0x2010).unwrap().battery_register, Some(0x0D));
        assert_eq!(
            register_reads(&sim.writes_to(Collection::Short, 2), 0x07),
            0
        );
    }

    #[test]
    fn warm_start_reads_the_remembered_register_directly() {
        let sim = SimReceiver::new().with(4, SimDevice::k800([0x07, 0x00, 0x00]));
        let mut profiles = DeviceProfiles::default();
        poll(&sim, short_link(&sim), &mut profiles);
        let before = sim.writes().len();

        let result = poll(&sim, short_link(&sim), &mut profiles);
        assert_eq!(summary(&result), [(4, K800_NAME, 90, false)]);
        let new_writes = sim.writes().split_off(before);
        // No ping, no enumeration: the first thing sent to slot 4 is the read.
        assert!(!new_writes
            .iter()
            .any(|(c, w)| *c == Collection::Long && w[1] == 4));
        let first = new_writes.iter().find(|(_, w)| w[1] == 4).unwrap();
        assert_eq!(first.1, [0x10, 4, 0x81, 0x07, 0, 0, 0]);
    }

    #[test]
    fn stale_register_profile_is_resolved_again() {
        let sim = SimReceiver::new().with(4, SimDevice::k800([0x03, 0x00, 0x00]));
        let mut profiles = DeviceProfiles::default();
        profiles.upsert(
            0x2010,
            BatterySource::Register(0x0D).to_profile("old".into()),
        );

        let result = poll(&sim, short_link(&sim), &mut profiles);
        assert_eq!(summary(&result), [(4, K800_NAME, 20, false)]);
        assert_eq!(profiles.get(0x2010).unwrap().battery_register, Some(0x07));
    }

    #[test]
    fn k800_is_found_by_its_ping_whatever_it_does_with_long_requests() {
        // Where a 1.0 device's rejection of a *long* 2.0 request lands is
        // unknown; detection uses a short ping, so it must not matter.
        for route in [ErrorRoute::Short, ErrorRoute::Long, ErrorRoute::Silent] {
            let device = hidpp10(&[(0x07, [0x05, 0x00, 0x00])], route);
            let sim = SimReceiver::new().with(4, device);
            let start = Instant::now();
            let result = poll(&sim, short_link(&sim), &mut DeviceProfiles::default());

            assert_eq!(summary(&result), [(4, K800_NAME, 50, false)], "{route:?}");
            assert!(sim.writes_to(Collection::Long, 4).is_empty(), "{route:?}");
            assert!(start.elapsed() < Duration::from_secs(1), "{route:?}");
        }
    }

    #[test]
    fn k800_charging_without_a_level_is_kept_until_it_reports_one() {
        let sim = SimReceiver::new().with(4, SimDevice::k800([0x00, 0x21, 0x00]));
        let mut profiles = DeviceProfiles::default();
        let result = poll(&sim, short_link(&sim), &mut profiles);
        // Not shown, but resolved and remembered: no level is not a stale profile.
        assert!(result.devices.is_empty());
        assert_eq!(result.errors.len(), 1, "{:?}", result.errors);
        assert_eq!(profiles.get(0x2010).unwrap().battery_register, Some(0x07));

        let before = sim.writes().len();
        sim.set_register(4, 0x07, [0x05, 0x21, 0x00]);
        let result = poll(&sim, short_link(&sim), &mut profiles);
        assert_eq!(summary(&result), [(4, K800_NAME, 50, true)]);
        // Straight from the profile: no new protocol ping.
        let pings = sim.writes().split_off(before);
        assert!(!pings.iter().any(|(_, w)| w[1] == 4 && w[2] == 0x00));
    }

    #[test]
    fn empty_k800_reads_as_zero() {
        let sim = SimReceiver::new().with(4, SimDevice::k800([0x00, 0x00, 0x00]));
        let result = poll(&sim, short_link(&sim), &mut DeviceProfiles::default());
        assert_eq!(summary(&result), [(4, K800_NAME, 0, false)]);
    }

    #[test]
    fn asleep_k800_fails_fast_and_keeps_its_profile() {
        let sim = SimReceiver::new().with(4, SimDevice::k800([0x07, 0x00, 0x00]));
        let mut profiles = DeviceProfiles::default();
        poll(&sim, short_link(&sim), &mut profiles);
        let before = sim.writes().len();

        sim.set_reachable(4, false);
        let start = Instant::now();
        let result = poll(&sim, short_link(&sim), &mut profiles);
        assert!(result.devices.is_empty());
        assert!(start.elapsed() < Duration::from_millis(500));
        assert_eq!(profiles.get(0x2010).unwrap().battery_register, Some(0x07));
        // No re-detection or enumeration for an asleep device.
        let writes = sim.writes().split_off(before);
        assert!(!writes
            .iter()
            .any(|(c, w)| w[1] == 4 && (*c == Collection::Long || w[2] == 0x00)));

        sim.set_reachable(4, true);
        let result = poll(&sim, short_link(&sim), &mut profiles);
        assert_eq!(summary(&result), [(4, K800_NAME, 90, false)]);
    }

    #[test]
    fn asleep_mouse_fails_fast_without_enumeration_or_register_reads() {
        let sim = SimReceiver::new().with(1, SimDevice::g502(68));
        sim.set_reachable(1, false);
        let start = Instant::now();
        let result = poll(&sim, short_link(&sim), &mut DeviceProfiles::default());

        assert!(result.devices.is_empty());
        assert!(
            result.errors[0].contains("not available"),
            "{:?}",
            result.errors
        );
        assert!(start.elapsed() < Duration::from_millis(500));
        assert!(sim.writes_to(Collection::Long, 1).is_empty());
        assert_eq!(register_requests(&sim.writes_to(Collection::Short, 1)), 0);
    }

    #[test]
    fn mouse_that_answered_its_ping_never_gets_a_register_probe() {
        let mouse = SimDevice::Hidpp20 {
            wpid: 0x4099,
            name: "G502 X PLUS",
            battery_percent: 68,
            flaky_first: 0,
            broken_features: true,
            replies_short: false,
        };
        let sim = SimReceiver::new().with(1, mouse);
        let mut profiles = DeviceProfiles::default();
        let result = poll(&sim, short_link(&sim), &mut profiles);

        assert!(result.devices.is_empty());
        assert_eq!(result.errors.len(), 1);
        assert_eq!(register_requests(&sim.writes_to(Collection::Short, 1)), 0);
        assert!(profiles.get(0x4099).is_none());
    }

    #[test]
    fn k800_that_takes_one_request_at_a_time_still_reads() {
        // With a slow link, a request sent before the previous answer arrived
        // is refused as busy; the notification setup must wait for its ack.
        let sim = SimReceiver::new()
            .with_latency(Duration::from_millis(30))
            .with(4, SimDevice::k800([0x05, 0x00, 0x00]));
        let result = poll(&sim, short_link(&sim), &mut DeviceProfiles::default());
        assert_eq!(
            summary(&result),
            [(4, K800_NAME, 50, false)],
            "{:?}",
            result.errors
        );
        assert_eq!(sim.register(4, 0x00), Some([0x10, 0x00, 0x00]));
    }

    #[test]
    fn late_register_reply_is_not_taken_for_the_next_read() {
        let sim = SimReceiver::new().with(4, SimDevice::k800([0x07, 0x00, 0x00]));
        let mut short = ShortLink::new(sim.link(Collection::Short));
        // The reply misses the read's window and arrives just after it.
        sim.delay_next_reply(4, Duration::from_millis(DEVICE_REQUEST_TIMEOUT_MS + 100));
        assert!(short.read_register(4, 0x07).is_err());

        sim.set_register(4, 0x07, [0x03, 0x00, 0x00]);
        thread::sleep(Duration::from_millis(200));
        assert_eq!(short.read_register(4, 0x07).unwrap(), [0x03, 0x00, 0x00]);
    }

    #[test]
    fn slow_k800_is_detected_and_read_without_resends() {
        // 750 ms answers: a resend would arrive while the device is busy.
        let sim = SimReceiver::new().with(4, SimDevice::k800([0x05, 0x00, 0x00]));
        sim.delay_next_reply(4, Duration::from_millis(750));
        let start = Instant::now();
        let result = poll(&sim, short_link(&sim), &mut DeviceProfiles::default());
        assert_eq!(
            summary(&result),
            [(4, K800_NAME, 50, false)],
            "{:?}",
            result.errors
        );
        // Detected by its (slow) ping answer, so no enumeration.
        assert!(sim.writes_to(Collection::Long, 4).is_empty());
        assert!(start.elapsed() < Duration::from_secs(2));

        let mut short = ShortLink::new(sim.link(Collection::Short));
        sim.delay_next_reply(4, Duration::from_millis(750));
        assert_eq!(short.read_register(4, 0x07).unwrap(), [0x05, 0x00, 0x00]);
        assert_eq!(
            register_reads(&sim.writes_to(Collection::Short, 4), 0x07),
            3
        );
    }

    #[test]
    fn full_k800_without_a_level_reads_as_full() {
        let sim = SimReceiver::new().with(4, SimDevice::k800([0x00, 0x22, 0x00]));
        let result = poll(&sim, short_link(&sim), &mut DeviceProfiles::default());
        assert_eq!(summary(&result), [(4, K800_NAME, 100, false)]);
    }

    #[test]
    fn mouse_answering_its_ping_short_is_still_hidpp20() {
        let mouse = SimDevice::Hidpp20 {
            wpid: 0x4099,
            name: "G502 X PLUS",
            battery_percent: 68,
            flaky_first: 0,
            broken_features: false,
            replies_short: true,
        };
        let sim = SimReceiver::new().with(1, mouse);
        let mut long = sim.link(Collection::Long);
        let mut short = ShortLink::new(sim.link(Collection::Short));
        let start = Instant::now();
        assert_eq!(
            detect_protocol(&mut *long, &mut short, 1),
            SlotProtocol::Hidpp20
        );
        assert!(start.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn busy_k800_does_not_settle_on_the_coarse_register() {
        let device = hidpp10(
            &[(0x0D, [0x55, 0x00, 0x30]), (0x07, [0x07, 0x00, 0x00])],
            ErrorRoute::Short,
        );
        let sim = SimReceiver::new().with(4, device);
        let mut profiles = DeviceProfiles::default();
        // The 0x0D probe is refused as busy: nothing is learned or remembered.
        sim.reject_next_register(4, receiver::ERR_BUSY);
        let result = poll(&sim, short_link(&sim), &mut profiles);
        assert!(result.devices.is_empty());
        assert!(profiles.get(0x2010).is_none());

        let result = poll(&sim, short_link(&sim), &mut profiles);
        assert_eq!(summary(&result), [(4, K800_NAME, 85, false)]);
        assert_eq!(profiles.get(0x2010).unwrap().battery_register, Some(0x0D));
    }

    #[test]
    fn full_backlog_keeps_the_newest_reports() {
        let mut backlog = VecDeque::new();
        for i in 0..SHORT_BACKLOG_CAP + 2 {
            push_bounded(&mut backlog, [0x10, 1, 0x41, 0, 0, 0, i as u8]);
        }
        assert_eq!(backlog.len(), SHORT_BACKLOG_CAP);
        assert_eq!(backlog.front().unwrap()[6], 2);
        assert_eq!(backlog.back().unwrap()[6], (SHORT_BACKLOG_CAP + 1) as u8);
    }

    #[test]
    fn one_ping_tells_the_protocols_apart() {
        let sim = SimReceiver::new()
            .with(1, SimDevice::g502(68))
            .with(4, SimDevice::k800([0x07, 0x00, 0x00]));
        let mut long = sim.link(Collection::Long);
        let mut short = ShortLink::new(sim.link(Collection::Short));
        assert_eq!(
            detect_protocol(&mut *long, &mut short, 1),
            SlotProtocol::Hidpp20
        );
        assert_eq!(
            detect_protocol(&mut *long, &mut short, 4),
            SlotProtocol::Hidpp10
        );
        assert_eq!(
            detect_protocol(&mut *long, &mut short, 3),
            SlotProtocol::Absent
        );
    }

    #[test]
    fn mouse_missing_its_pings_is_still_read_as_hidpp20() {
        let mouse = SimDevice::Hidpp20 {
            wpid: 0x4099,
            name: "G502 X PLUS",
            battery_percent: 68,
            flaky_first: PING_RETRIES,
            broken_features: false,
            replies_short: false,
        };
        let sim = SimReceiver::new().with(1, mouse);
        let mut profiles = DeviceProfiles::default();
        let result = poll(&sim, short_link(&sim), &mut profiles);

        assert_eq!(summary(&result), [(1, "G502 X PLUS", 68, false)]);
        assert_eq!(profiles.get(0x4099).unwrap().battery_register, None);
        assert_eq!(register_requests(&sim.writes_to(Collection::Short, 1)), 0);
    }

    #[test]
    #[ignore = "slow: waits out the ping and enumeration timeouts (~15 s)"]
    fn unresponsive_mouse_is_never_taken_for_a_register_device() {
        let mouse = SimDevice::Hidpp20 {
            wpid: 0x4099,
            name: "G502 X PLUS",
            battery_percent: 68,
            flaky_first: usize::MAX,
            broken_features: false,
            replies_short: false,
        };
        let sim = SimReceiver::new().with(1, mouse);
        let mut profiles = DeviceProfiles::default();
        let result = poll(&sim, short_link(&sim), &mut profiles);

        assert!(result.devices.is_empty());
        assert_eq!(result.errors.len(), 1);
        assert!(profiles.get(0x4099).is_none());
    }

    #[test]
    fn without_a_short_collection_the_mouse_still_reads() {
        let sim = SimReceiver::new()
            .with(1, SimDevice::g502(68))
            .with(4, SimDevice::k800([0x07, 0x00, 0x00]));
        let result = poll(&sim, None, &mut DeviceProfiles::default());
        // The K800 cannot be reached without the short collection; the mouse
        // must not suffer for it.
        assert_eq!(summary(&result), [(1, "G502 X PLUS", 68, false)]);
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
    fn worker_keeps_the_mouse_announcement_that_lands_during_k800_reads() {
        // Announcements go out by slot, so the mouse's 0x41 is already queued
        // on the short collection while the K800's registers are read there.
        let sim = SimReceiver::new()
            .with(1, SimDevice::k800([0x05, 0x00, 0x00]))
            .with(2, SimDevice::g502(68));
        let worker = Worker::start(&sim);
        let mut seen = BTreeSet::new();
        while seen.len() < 2 {
            if let DeviceEvent::Update(state) = worker.next() {
                seen.insert((state.device_index, state.battery_percent));
            }
        }
        assert_eq!(seen, BTreeSet::from([(1, 50), (2, 68)]));
    }

    #[test]
    fn worker_rereads_a_k800_that_pushes_a_battery_change() {
        let sim = SimReceiver::new().with(4, SimDevice::k800([0x07, 0x00, 0x00]));
        let worker = Worker::start(&sim);
        assert_eq!(worker.update_for(4).battery_percent, 90);

        assert!(sim.battery_change(4, [0x03, 0x21, 0x00]));
        let state = worker.update_for(4);
        assert_eq!((state.battery_percent, state.is_charging), (20, true));
    }

    #[test]
    fn worker_reports_a_k800_link_down() {
        let sim = SimReceiver::new().with(4, SimDevice::k800([0x07, 0x00, 0x00]));
        let worker = Worker::start(&sim);
        let state = worker.update_for(4);

        sim.push(Collection::Short, &[0x10, 4, 0x41, 0x04, 0x41, 0x10, 0x20]);
        loop {
            if let DeviceEvent::Gone(key) = worker.next() {
                assert_eq!(key, state.device_key);
                break;
            }
        }
    }

    #[test]
    fn worker_shows_a_charging_k800_once_it_pushes_a_level() {
        let sim = SimReceiver::new().with(4, SimDevice::k800([0x00, 0x21, 0x00]));
        let worker = Worker::start(&sim);
        // Nothing to show yet, but the slot must stay known for its pushes.
        assert!(worker
            .events
            .recv_timeout(Duration::from_millis(500))
            .is_err());

        // Docked with no level, but its notifications were switched on: the
        // push only arrives because of that.
        assert!(sim.battery_change(4, [0x05, 0x21, 0x00]));
        let state = worker.update_for(4);
        assert_eq!((state.battery_percent, state.is_charging), (50, true));

        // A charging push without a level keeps the last one shown.
        assert!(sim.battery_change(4, [0x00, 0x21, 0x00]));
        assert_eq!(worker.update_for(4).battery_percent, 50);
    }

    #[test]
    fn worker_ignores_short_events_from_hidpp20_devices() {
        // Feature index 7 on the short collection looks like a 1.0 battery push.
        let sim = SimReceiver::new().with(1, SimDevice::g502(68));
        let worker = Worker::start(&sim);
        worker.update_for(1);
        let before = sim.writes_to(Collection::Long, 1).len();

        sim.push(Collection::Short, &[0x10, 1, 0x07, 0x00, 0x00, 0x00, 0x00]);
        assert!(worker
            .events
            .recv_timeout(Duration::from_millis(1500))
            .is_err());
        assert_eq!(sim.writes_to(Collection::Long, 1).len(), before);
    }

    fn k800_profile() -> DeviceProfiles {
        let mut profiles = DeviceProfiles::default();
        profiles.upsert(
            0x2010,
            BatterySource::Register(0x07).to_profile(K800_NAME.into()),
        );
        profiles
    }

    /// A battery change the device must push (its notifications are on).
    fn push_battery(sim: &SimReceiver, slot: u8, value: [u8; 3]) {
        assert!(sim.battery_change(slot, value), "notifications are off");
    }

    #[test]
    fn worker_shows_a_charged_k800_as_full() {
        let sim = SimReceiver::new().with(4, SimDevice::k800([0x01, 0x00, 0x00]));
        let worker = Worker::start(&sim);
        assert_eq!(worker.update_for(4).battery_percent, 5);

        push_battery(&sim, 4, [0x00, 0x21, 0x00]);
        let state = worker.update_for(4);
        assert_eq!((state.battery_percent, state.is_charging), (5, true));

        push_battery(&sim, 4, [0x00, 0x22, 0x00]);
        let state = worker.update_for(4);
        assert_eq!((state.battery_percent, state.is_charging), (100, false));
    }

    #[test]
    fn worker_does_not_reuse_a_level_once_the_charge_ended() {
        let sim = SimReceiver::new().with(4, SimDevice::k800([0x05, 0x00, 0x00]));
        let worker = Worker::start(&sim);
        assert_eq!(worker.update_for(4).battery_percent, 50);

        // A level code we do not understand, not charging: show nothing new
        // rather than the old level.
        push_battery(&sim, 4, [0x04, 0x00, 0x00]);
        assert!(worker
            .events
            .recv_timeout(Duration::from_millis(500))
            .is_err());
        // The pushes are being handled: a readable level shows up again.
        push_battery(&sim, 4, [0x03, 0x00, 0x00]);
        assert_eq!(worker.update_for(4).battery_percent, 20);
    }

    #[test]
    fn docked_k800_without_a_level_still_gets_its_notifications_on() {
        let sim = SimReceiver::new().with(4, SimDevice::k800([0x00, 0x21, 0x00]));
        let result = poll(&sim, short_link(&sim), &mut DeviceProfiles::default());
        assert!(result.devices.is_empty());
        assert_eq!(sim.register(4, 0x00), Some([0x10, 0x00, 0x00]));
    }

    #[test]
    fn worker_stops_asking_a_k800_without_a_notification_register() {
        let sim =
            SimReceiver::new().with(4, hidpp10(&[(0x07, [0x07, 0x00, 0x00])], ErrorRoute::Short));
        // This one has no register 0x00 at all.
        sim.remove_register(4, 0x00);
        let worker = Worker::start(&sim);
        worker.update_for(4);
        worker.cmd.send(WorkerCommand::Refresh).unwrap();
        worker.update_for(4);
        let asked = register_reads(&sim.writes_to(Collection::Short, 4), 0x00);
        assert_eq!(asked, 1);
    }

    #[test]
    fn worker_does_not_re_resolve_a_failing_mouse_on_its_short_events() {
        let mouse = SimDevice::Hidpp20 {
            wpid: 0x4099,
            name: "G502 X PLUS",
            battery_percent: 68,
            flaky_first: 0,
            broken_features: true,
            replies_short: false,
        };
        let sim = SimReceiver::new().with(1, mouse);
        let worker = Worker::start(&sim);
        assert!(worker
            .events
            .recv_timeout(Duration::from_millis(500))
            .is_err());
        let before = sim.writes().len();

        sim.push(Collection::Short, &[0x10, 1, 0x07, 0x00, 0x00, 0x00, 0x00]);
        thread::sleep(Duration::from_millis(1000));
        assert_eq!(sim.writes().len(), before);
    }

    #[test]
    fn k800_busy_at_its_ping_skips_enumeration() {
        let sim = SimReceiver::new().with(4, SimDevice::k800([0x07, 0x00, 0x00]));
        sim.reject_next_ping(4, receiver::ERR_BUSY);
        let start = Instant::now();
        let result = poll(&sim, short_link(&sim), &mut DeviceProfiles::default());
        assert!(
            result.errors[0].contains("not available"),
            "{:?}",
            result.errors
        );
        assert!(start.elapsed() < Duration::from_millis(500));
        assert!(sim.writes_to(Collection::Long, 4).is_empty());

        let result = poll(&sim, short_link(&sim), &mut DeviceProfiles::default());
        assert_eq!(summary(&result), [(4, K800_NAME, 90, false)]);
    }

    #[test]
    fn worker_reenables_k800_notifications_on_warm_start_and_reconnect() {
        let sim = SimReceiver::new().with(4, SimDevice::k800([0x07, 0x00, 0x00]));
        let worker = Worker::start_with(&sim, k800_profile(), true, 60);
        worker.update_for(4);
        assert_eq!(sim.register(4, 0x00), Some([0x10, 0x00, 0x00]));
        // Warm: straight from the profile, no protocol ping.
        assert!(!sim
            .writes_to(Collection::Short, 4)
            .iter()
            .any(|w| w[2] == 0x00));

        // Power cycle: link down, the device forgets its flags, link up.
        sim.push(Collection::Short, &[0x10, 4, 0x41, 0x04, 0x41, 0x10, 0x20]);
        while !matches!(worker.next(), DeviceEvent::Gone(_)) {}
        sim.set_register(4, 0x00, [0x00, 0x00, 0x00]);
        sim.push(Collection::Short, &[0x10, 4, 0x41, 0x04, 0x01, 0x10, 0x20]);
        worker.update_for(4);
        assert_eq!(sim.register(4, 0x00), Some([0x10, 0x00, 0x00]));
    }

    #[test]
    fn worker_picks_up_a_k800_that_was_asleep_at_a_warm_start() {
        let sim = SimReceiver::new().with(4, SimDevice::k800([0x07, 0x00, 0x00]));
        // Its notifications are still on from an earlier session.
        sim.set_register(4, 0x00, [0x10, 0x00, 0x00]);
        sim.set_reachable(4, false);
        let worker = Worker::start_with(&sim, k800_profile(), true, 60);
        assert!(worker
            .events
            .recv_timeout(Duration::from_millis(500))
            .is_err());

        sim.set_reachable(4, true);
        push_battery(&sim, 4, [0x05, 0x00, 0x00]);
        assert_eq!(worker.update_for(4).battery_percent, 50);
    }

    #[test]
    fn worker_picks_up_a_k800_that_was_asleep_at_a_cold_start() {
        let sim = SimReceiver::new().with(4, SimDevice::k800([0x07, 0x00, 0x00]));
        // Its notifications are still on from an earlier session (it has not
        // powered off since); without them it cannot push and only the next
        // announcement picks it up.
        sim.set_register(4, 0x00, [0x10, 0x00, 0x00]);
        sim.set_reachable(4, false);
        let worker = Worker::start(&sim);
        assert!(worker
            .events
            .recv_timeout(Duration::from_millis(500))
            .is_err());

        sim.set_reachable(4, true);
        push_battery(&sim, 4, [0x03, 0x00, 0x00]);
        let state = worker.update_for(4);
        assert_eq!(
            (state.display_name.as_str(), state.battery_percent),
            (K800_NAME, 20)
        );
    }

    #[test]
    fn worker_without_a_short_collection_finds_the_mouse() {
        let sim = SimReceiver::new().with(1, SimDevice::g502(68));
        let worker = Worker::start_with(&sim, DeviceProfiles::default(), false, 60);
        assert_eq!(worker.update_for(1).battery_percent, 68);
    }

    #[test]
    #[ignore = "slow: waits for the 15 s minimum safety interval"]
    fn worker_safety_tick_rereads_a_k800() {
        let sim = SimReceiver::new().with(4, SimDevice::k800([0x07, 0x00, 0x00]));
        let worker = Worker::start_with(&sim, DeviceProfiles::default(), true, MIN_SAFETY_SECS);
        assert_eq!(worker.update_for(4).battery_percent, 90);

        // A change the device does not push: only the safety re-read sees it.
        sim.set_register(4, 0x07, [0x03, 0x00, 0x00]);
        let deadline = Instant::now() + Duration::from_secs(MIN_SAFETY_SECS + 5);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match worker.events.recv_timeout(remaining) {
                Ok(DeviceEvent::Update(state)) if state.battery_percent == 20 => break,
                Ok(_) => {}
                Err(err) => panic!("no safety re-read: {err}"),
            }
        }
        // The receiver's flags were re-asserted by the tick (once at start).
        let flags = receiver::notification_flags();
        let flag_writes = sim
            .writes_to(Collection::Short, 0xFF)
            .iter()
            .filter(|w| w[2] == 0x80 && w[3] == 0x00 && w[4..7] == flags)
            .count();
        assert!(flag_writes >= 2, "{flag_writes}");
    }
}

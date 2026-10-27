//! A simulated receiver for tests: up to six paired devices behind the two HID
//! collections Windows exposes (short 0x10 reports, long 0x11 reports).
//!
//! Routing follows what a C547 does on real hardware (`--diag` register probe):
//! every 0x10 frame, HID++ 1.0 errors included, is delivered on the short
//! collection and every 0x11 frame on the long one. A register read to a HID++
//! 2.0 device is rejected with 0x01, one to an empty slot with 0x08; a 2.0 ping
//! to an empty slot gets a short 0x08 error whether it was sent short or long,
//! and a 2.0 device answers a ping sent short with a long reply.
//!
//! The HID++ 1.0 device's behaviour is modelled on Solaar and is the part we
//! cannot check against hardware: it rejects a 2.0 request sent short with
//! 0x01 (what Solaar's ping relies on), [`ErrorRoute`] covers the open question
//! of a 2.0 request sent long, and it answers one request at a time (0x07 busy
//! while a reply is outstanding).

use crate::hid::link::HidLink;
use anyhow::Result;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Collection {
    Short,
    Long,
}

/// Where a HID++ 1.0 device's rejection of a HID++ 2.0 request sent *long*
/// ends up. (Sent short, it is a short 0x01 error, as Solaar documents.)
#[derive(Clone, Copy, Debug)]
pub enum ErrorRoute {
    /// As a short 0x8F error on the short collection (what the C547 does for
    /// errors it generates itself).
    Short,
    /// As a long-framed 0x8F error on the long collection.
    Long,
    /// Nowhere.
    Silent,
}

#[derive(Clone, Debug)]
pub enum SimDevice {
    /// A HID++ 1.0 device with these readable/writable registers.
    Hidpp10 {
        wpid: u16,
        registers: HashMap<u8, [u8; 3]>,
        error_route: ErrorRoute,
    },
    /// A HID++ 2.0 device with features root, feature set, name (0x0005) and
    /// unified battery (0x1004). The first `flaky_first` requests go
    /// unanswered, like a device still waking up. With `broken_features`,
    /// every request but a ping is answered with a HID++ 2.0 error. With
    /// `replies_short`, a request sent short is answered short (Solaar accepts
    /// either; the G502 answers long).
    Hidpp20 {
        wpid: u16,
        name: &'static str,
        battery_percent: u8,
        flaky_first: usize,
        broken_features: bool,
        replies_short: bool,
    },
}

impl SimDevice {
    /// The original K800: battery register 0x07 only (as Solaar describes it),
    /// plus the notification register 0x00.
    pub fn k800(battery_status: [u8; 3]) -> Self {
        Self::Hidpp10 {
            wpid: 0x2010,
            registers: HashMap::from([(0x07, battery_status), (0x00, [0, 0, 0])]),
            error_route: ErrorRoute::Short,
        }
    }

    pub fn g502(battery_percent: u8) -> Self {
        Self::mouse(0x4099, "G502 X PLUS", battery_percent)
    }

    pub fn mouse(wpid: u16, name: &'static str, battery_percent: u8) -> Self {
        Self::Hidpp20 {
            wpid,
            name,
            battery_percent,
            flaky_first: 0,
            broken_features: false,
            replies_short: false,
        }
    }

    /// Leave the first `count` requests unanswered (HID++ 2.0 devices).
    pub fn flaky(mut self, count: usize) -> Self {
        if let Self::Hidpp20 { flaky_first, .. } = &mut self {
            *flaky_first = count;
        }
        self
    }

    fn wpid(&self) -> u16 {
        match self {
            Self::Hidpp10 { wpid, .. } | Self::Hidpp20 { wpid, .. } => *wpid,
        }
    }

    /// Device kind nibble of the 0x41 notification (1 keyboard, 2 mouse).
    fn kind(&self) -> u8 {
        match self {
            Self::Hidpp10 { .. } => 0x01,
            Self::Hidpp20 { .. } => 0x02,
        }
    }
}

/// Feature table of a simulated HID++ 2.0 device, by index.
const FEATURES: [u16; 4] = [0x0000, 0x0001, 0x0005, 0x1004];

/// HID++ 1.0 error codes the simulator uses.
const ERR_INVALID_SUBID: u8 = 0x01;
const ERR_INVALID_ADDRESS: u8 = 0x02;
const ERR_BUSY: u8 = 0x07;
const ERR_UNKNOWN_DEVICE: u8 = 0x08;
const ERR_RESOURCE_ERROR: u8 = 0x09;

#[derive(Default)]
struct State {
    slots: [Option<SimDevice>; 7],
    short_queue: VecDeque<(Instant, Vec<u8>)>,
    long_queue: VecDeque<(Instant, Vec<u8>)>,
    latency: Duration,
    writes: Vec<(Collection, Vec<u8>)>,
    /// Paired but asleep: the receiver answers for them with 0x09.
    unreachable: HashSet<u8>,
    /// When a HID++ 1.0 device's outstanding reply is delivered.
    busy_until: HashMap<u8, Instant>,
    /// Extra delay for the next reply from a slot (one-shot).
    delay_next: HashMap<u8, Duration>,
    /// Error code for the next register request to a slot (one-shot).
    reject_next: HashMap<u8, u8>,
    /// Error code for the next ping to a slot (one-shot).
    reject_ping: HashMap<u8, u8>,
}

/// The receiver. Clones share state; hand out its collections with [`link`].
#[derive(Clone, Default)]
pub struct SimReceiver(Arc<Mutex<State>>);

impl SimReceiver {
    pub fn new() -> Self {
        let sim = Self::default();
        sim.state().latency = Duration::from_millis(2);
        sim
    }

    pub fn with(self, slot: u8, device: SimDevice) -> Self {
        self.state().slots[slot as usize] = Some(device);
        self
    }

    pub fn with_latency(self, latency: Duration) -> Self {
        self.state().latency = latency;
        self
    }

    pub fn link(&self, collection: Collection) -> Box<dyn HidLink> {
        Box::new(SimLink {
            sim: self.clone(),
            collection,
        })
    }

    /// Deliver an unsolicited report.
    pub fn push(&self, collection: Collection, report: &[u8]) {
        let mut state = self.state();
        let due = Instant::now() + state.latency;
        state.queue(collection, due, report.to_vec());
    }

    /// Every report written so far, with the collection it was written to.
    pub fn writes(&self) -> Vec<(Collection, Vec<u8>)> {
        self.state().writes.clone()
    }

    /// Writes addressed to `slot` (byte 1) on `collection`.
    pub fn writes_to(&self, collection: Collection, slot: u8) -> Vec<Vec<u8>> {
        self.writes()
            .into_iter()
            .filter(|(c, w)| *c == collection && w[1] == slot)
            .map(|(_, w)| w)
            .collect()
    }

    /// A HID++ 1.0 device's register value.
    pub fn register(&self, slot: u8, register: u8) -> Option<[u8; 3]> {
        match &self.state().slots[slot as usize] {
            Some(SimDevice::Hidpp10 { registers, .. }) => registers.get(&register).copied(),
            _ => None,
        }
    }

    pub fn set_battery(&self, slot: u8, battery_percent: u8) {
        if let Some(SimDevice::Hidpp20 {
            battery_percent: level,
            ..
        }) = &mut self.state().slots[slot as usize]
        {
            *level = battery_percent;
        }
    }

    pub fn set_register(&self, slot: u8, register: u8, value: [u8; 3]) {
        if let Some(SimDevice::Hidpp10 { registers, .. }) = &mut self.state().slots[slot as usize] {
            registers.insert(register, value);
        }
    }

    pub fn remove_register(&self, slot: u8, register: u8) {
        if let Some(SimDevice::Hidpp10 { registers, .. }) = &mut self.state().slots[slot as usize] {
            registers.remove(&register);
        }
    }

    /// A HID++ 1.0 device's battery register changes to `value`; the device
    /// pushes it only if its battery notifications (register 0x00, bit 0x10 of
    /// the first byte) are on, as Solaar's flag model describes. Returns
    /// whether it pushed.
    pub fn battery_change(&self, slot: u8, value: [u8; 3]) -> bool {
        self.set_register(slot, 0x07, value);
        let enabled = self
            .register(slot, 0x00)
            .is_some_and(|flags| flags[0] & 0x10 != 0);
        if enabled {
            self.push(
                Collection::Short,
                &[0x10, slot, 0x07, value[0], value[1], value[2], 0x00],
            );
        }
        enabled
    }

    pub fn set_reachable(&self, slot: u8, reachable: bool) {
        let mut state = self.state();
        if reachable {
            state.unreachable.remove(&slot);
        } else {
            state.unreachable.insert(slot);
        }
    }

    /// Hold the next reply from `slot` back by `delay`.
    pub fn delay_next_reply(&self, slot: u8, delay: Duration) {
        self.state().delay_next.insert(slot, delay);
    }

    /// Answer the next register request to `slot` with error `code`.
    pub fn reject_next_register(&self, slot: u8, code: u8) {
        self.state().reject_next.insert(slot, code);
    }

    /// Answer the next ping to `slot` with error `code`.
    pub fn reject_next_ping(&self, slot: u8, code: u8) {
        self.state().reject_ping.insert(slot, code);
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap()
    }
}

impl State {
    fn queue(&mut self, collection: Collection, due: Instant, report: Vec<u8>) {
        match collection {
            Collection::Short => self.short_queue.push_back((due, report)),
            Collection::Long => self.long_queue.push_back((due, report)),
        }
    }

    /// Deliver an answer for `slot`, routed by its report id as Windows does.
    fn answer(&mut self, slot: u8, frame: Vec<u8>) {
        let extra = self.delay_next.remove(&slot).unwrap_or_default();
        let due = Instant::now() + self.latency + extra;
        if matches!(
            self.slots.get(slot as usize),
            Some(Some(SimDevice::Hidpp10 { .. }))
        ) {
            self.busy_until.insert(slot, due);
        }
        let collection = if frame[0] == 0x10 {
            Collection::Short
        } else {
            Collection::Long
        };
        self.queue(collection, due, frame);
    }

    fn short_frame(bytes: [u8; 7]) -> Vec<u8> {
        bytes.to_vec()
    }

    fn long_frame(head: &[u8]) -> Vec<u8> {
        let mut frame = vec![0u8; 20];
        frame[..head.len()].copy_from_slice(head);
        frame
    }

    /// The error the receiver or device gives instead of handling a request
    /// at all: empty slot, asleep, or a 1.0 device still busy with the last.
    fn refusal(&self, slot: u8) -> Option<u8> {
        match self.slots.get(slot as usize).and_then(Option::as_ref) {
            None => Some(ERR_UNKNOWN_DEVICE),
            Some(_) if self.unreachable.contains(&slot) => Some(ERR_RESOURCE_ERROR),
            Some(SimDevice::Hidpp10 { .. })
                if self
                    .busy_until
                    .get(&slot)
                    .is_some_and(|until| Instant::now() < *until) =>
            {
                Some(ERR_BUSY)
            }
            Some(_) => None,
        }
    }

    fn write_short(&mut self, w: &[u8]) {
        let (slot, sub, reg) = (w[1], w[2], w[3]);
        let params = [w[4], w[5], w[6]];
        if slot == 0xFF {
            self.receiver_register(sub, reg, params);
            return;
        }
        if sub < 0x80 {
            // A HID++ 2.0 request sent as a short report (e.g. Solaar's ping).
            self.hidpp20_request(w, Collection::Short);
            return;
        }
        let error = |code| Self::short_frame([0x10, slot, 0x8F, sub, reg, code, 0]);
        if let Some(code) = self.refusal(slot) {
            self.answer(slot, error(code));
            return;
        }
        if let Some(code) = self.reject_next.remove(&slot) {
            self.answer(slot, error(code));
            return;
        }
        let reply = match self.slots[slot as usize].as_mut() {
            Some(SimDevice::Hidpp10 { registers, .. }) => match sub {
                0x81 => match registers.get(&reg) {
                    Some(v) => Self::short_frame([0x10, slot, 0x81, reg, v[0], v[1], v[2]]),
                    None => error(ERR_INVALID_ADDRESS),
                },
                0x80 if registers.contains_key(&reg) => {
                    registers.insert(reg, params);
                    Self::short_frame([0x10, slot, 0x80, reg, 0, 0, 0])
                }
                _ => error(ERR_INVALID_ADDRESS),
            },
            // Verified on a G502: register requests are rejected as 0x01.
            _ => error(ERR_INVALID_SUBID),
        };
        self.answer(slot, reply);
    }

    fn receiver_register(&mut self, sub: u8, reg: u8, params: [u8; 3]) {
        let ack = |reply: [u8; 3]| {
            Self::short_frame([0x10, 0xFF, sub, reg, reply[0], reply[1], reply[2]])
        };
        match (sub, reg) {
            (0x80, 0x00) => self.answer(0xFF, ack([0, 0, 0])),
            (0x81, 0x02) => {
                let count = self.slots.iter().flatten().count() as u8;
                self.answer(0xFF, ack([0, count, 0]));
            }
            (0x80, 0x02) => {
                self.answer(0xFF, ack([0, 0, 0]));
                if params[0] == 0x02 {
                    let announced: Vec<_> = (1..=6u8)
                        .filter_map(|s| self.slots[s as usize].as_ref().map(|d| (s, d.clone())))
                        .collect();
                    for (slot, device) in announced {
                        let wpid = device.wpid();
                        let due = Instant::now() + self.latency;
                        self.queue(
                            Collection::Short,
                            due,
                            Self::short_frame([
                                0x10,
                                slot,
                                0x41,
                                0x04,
                                device.kind(),
                                wpid as u8,
                                (wpid >> 8) as u8,
                            ]),
                        );
                    }
                }
            }
            _ => self.answer(
                0xFF,
                Self::short_frame([0x10, 0xFF, 0x8F, sub, reg, 0x02, 0]),
            ),
        }
    }

    /// A HID++ 2.0 request (feature index, function|software id), sent short
    /// or long.
    fn hidpp20_request(&mut self, w: &[u8], sent_on: Collection) {
        let (slot, feature, function_byte) = (w[1], w[2], w[3]);
        if slot == 0xFF {
            return;
        }
        let short_error =
            |code| Self::short_frame([0x10, slot, 0x8F, feature, function_byte, code, 0]);
        if let Some(code) = self.refusal(slot) {
            self.answer(slot, short_error(code));
            return;
        }
        if feature == 0 && function_byte >> 4 == 1 {
            if let Some(code) = self.reject_ping.remove(&slot) {
                self.answer(slot, short_error(code));
                return;
            }
        }
        let short_reply = matches!(
            self.slots[slot as usize],
            Some(SimDevice::Hidpp20 {
                replies_short: true,
                ..
            })
        );
        let reply = match self.slots[slot as usize].as_mut() {
            Some(SimDevice::Hidpp10 { error_route, .. }) => match (sent_on, *error_route) {
                (Collection::Short, _) | (Collection::Long, ErrorRoute::Short) => {
                    Some(short_error(ERR_INVALID_SUBID))
                }
                (Collection::Long, ErrorRoute::Long) => Some(Self::long_frame(&[
                    0x11,
                    slot,
                    0x8F,
                    feature,
                    function_byte,
                    ERR_INVALID_SUBID,
                ])),
                (Collection::Long, ErrorRoute::Silent) => None,
            },
            Some(SimDevice::Hidpp20 {
                name,
                battery_percent,
                flaky_first,
                broken_features,
                ..
            }) => {
                if *flaky_first > 0 {
                    *flaky_first -= 1;
                    None
                } else if *broken_features && !(feature == 0 && function_byte >> 4 == 1) {
                    Some(Self::long_frame(&[
                        0x11,
                        slot,
                        0xFF,
                        feature,
                        function_byte,
                        0x05,
                    ]))
                } else {
                    Some(Self::feature_reply(
                        slot,
                        feature,
                        function_byte,
                        [w[4], w[5], w[6]],
                        name,
                        *battery_percent,
                    ))
                }
            }
            None => None,
        };
        if let Some(mut reply) = reply {
            if sent_on == Collection::Short && short_reply && reply[0] == 0x11 {
                reply.truncate(7);
                reply[0] = 0x10;
            }
            self.answer(slot, reply);
        }
    }

    fn feature_reply(
        slot: u8,
        feature: u8,
        function_byte: u8,
        params: [u8; 3],
        name: &str,
        battery_percent: u8,
    ) -> Vec<u8> {
        let head = [0x11, slot, feature, function_byte];
        let reply = |payload: &[u8]| Self::long_frame(&[&head[..], payload].concat());
        match (FEATURES.get(feature as usize), function_byte >> 4) {
            // Root: getFeature(id) -> index, or 0 when absent.
            (Some(0x0000), 0) => {
                let id = u16::from_be_bytes([params[0], params[1]]);
                let index = FEATURES.iter().position(|&f| f == id).unwrap_or(0);
                reply(&[index as u8])
            }
            // Root: ping -> protocol 4.2, echo.
            (Some(0x0000), 1) => reply(&[4, 2, params[2]]),
            // Feature set: count (root excluded), then id at index.
            (Some(0x0001), 0) => reply(&[(FEATURES.len() - 1) as u8]),
            (Some(0x0001), 1) => {
                let id = FEATURES.get(params[0] as usize).copied().unwrap_or(0);
                reply(&id.to_be_bytes())
            }
            // Device name: length, then chunks from an offset.
            (Some(0x0005), 0) => reply(&[name.len() as u8]),
            (Some(0x0005), 1) => {
                let start = (params[0] as usize).min(name.len());
                reply(&name.as_bytes()[start..])
            }
            // Unified battery: get status -> percent, level, discharging.
            (Some(0x1004), 1) => reply(&[battery_percent, 0, 0]),
            _ => Self::long_frame(&[0x11, slot, 0xFF, feature, function_byte, 0x02]),
        }
    }
}

struct SimLink {
    sim: SimReceiver,
    collection: Collection,
}

impl HidLink for SimLink {
    fn write(&self, data: &[u8]) -> Result<usize> {
        let mut state = self.sim.state();
        state.writes.push((self.collection, data.to_vec()));
        // A receiver only takes each report on the collection that declares it.
        match (self.collection, data[0]) {
            (Collection::Short, 0x10) => state.write_short(data),
            (Collection::Long, 0x11) => state.hidpp20_request(data, Collection::Long),
            _ => {}
        }
        Ok(data.len())
    }

    fn read_timeout(&self, buf: &mut [u8], timeout_ms: i32) -> Result<usize> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(0) as u64);
        loop {
            {
                let mut state = self.sim.state();
                let queue = match self.collection {
                    Collection::Short => &mut state.short_queue,
                    Collection::Long => &mut state.long_queue,
                };
                // Reports are delivered in order of arrival time.
                let now = Instant::now();
                if let Some(index) = queue
                    .iter()
                    .enumerate()
                    .filter(|(_, (due, _))| *due <= now)
                    .min_by_key(|(_, (due, _))| *due)
                    .map(|(index, _)| index)
                {
                    let (_, report) = queue.remove(index).unwrap();
                    let n = report.len().min(buf.len());
                    buf[..n].copy_from_slice(&report[..n]);
                    return Ok(n);
                }
            }
            if Instant::now() >= deadline {
                return Ok(0);
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
}

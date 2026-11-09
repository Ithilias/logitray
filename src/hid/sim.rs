//! A simulated receiver for tests: up to six paired devices behind the two HID
//! collections Windows exposes (short 0x10 reports, long 0x11 reports).
//!
//! Routing follows what a C547 does on real hardware (`--diag`): every 0x10
//! frame, HID++ 1.0 errors included, is delivered on the short collection and
//! every 0x11 frame on the long one. A 2.0 request to an empty slot gets a short
//! 0x08 error. The receiver answers its own registers (notification flags,
//! connected count) and announces each paired device with a 0x41 report.

use crate::hid::link::HidLink;
use anyhow::Result;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Collection {
    Short,
    Long,
}

#[derive(Clone, Debug)]
pub enum SimDevice {
    /// A HID++ 2.0 device with features root, feature set, name (0x0005) and
    /// unified battery (0x1004). The first `flaky_first` requests go
    /// unanswered, like a device still waking up.
    Hidpp20 {
        wpid: u16,
        name: &'static str,
        battery_percent: u8,
        flaky_first: usize,
    },
}

impl SimDevice {
    pub fn g502(battery_percent: u8) -> Self {
        Self::mouse(0x4099, "G502 X PLUS", battery_percent)
    }

    pub fn mouse(wpid: u16, name: &'static str, battery_percent: u8) -> Self {
        Self::Hidpp20 {
            wpid,
            name,
            battery_percent,
            flaky_first: 0,
        }
    }

    /// Leave the first `count` requests unanswered.
    pub fn flaky(mut self, count: usize) -> Self {
        let Self::Hidpp20 { flaky_first, .. } = &mut self;
        *flaky_first = count;
        self
    }

    fn wpid(&self) -> u16 {
        match self {
            Self::Hidpp20 { wpid, .. } => *wpid,
        }
    }
}

/// Feature table of a simulated device, by index.
const FEATURES: [u16; 4] = [0x0000, 0x0001, 0x0005, 0x1004];

/// HID++ 1.0 error codes the simulator uses.
const ERR_INVALID_ADDRESS: u8 = 0x02;
const ERR_UNKNOWN_DEVICE: u8 = 0x08;

#[derive(Default)]
struct State {
    slots: [Option<SimDevice>; 7],
    short_queue: VecDeque<(Instant, Vec<u8>)>,
    long_queue: VecDeque<(Instant, Vec<u8>)>,
    latency: Duration,
    writes: Vec<(Collection, Vec<u8>)>,
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

    pub fn link(&self, collection: Collection) -> Box<dyn HidLink> {
        Box::new(SimLink {
            sim: self.clone(),
            collection,
        })
    }

    /// Deliver an unsolicited report.
    pub fn push(&self, collection: Collection, report: &[u8]) {
        self.state().deliver(collection, report.to_vec());
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

    pub fn set_battery(&self, slot: u8, battery_percent: u8) {
        if let Some(SimDevice::Hidpp20 {
            battery_percent: level,
            ..
        }) = &mut self.state().slots[slot as usize]
        {
            *level = battery_percent;
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap()
    }
}

impl State {
    /// Deliver a frame after the link latency, routed by its report id as
    /// Windows does.
    fn deliver(&mut self, collection: Collection, frame: Vec<u8>) {
        let due = Instant::now() + self.latency;
        match collection {
            Collection::Short => self.short_queue.push_back((due, frame)),
            Collection::Long => self.long_queue.push_back((due, frame)),
        }
    }

    fn answer(&mut self, frame: Vec<u8>) {
        let collection = if frame[0] == 0x10 {
            Collection::Short
        } else {
            Collection::Long
        };
        self.deliver(collection, frame);
    }

    fn long_frame(head: &[u8]) -> Vec<u8> {
        let mut frame = vec![0u8; 20];
        frame[..head.len()].copy_from_slice(head);
        frame
    }

    fn write_short(&mut self, w: &[u8]) {
        let (slot, sub, reg) = (w[1], w[2], w[3]);
        if slot == 0xFF {
            self.receiver_register(sub, reg, w[4]);
        }
    }

    fn receiver_register(&mut self, sub: u8, reg: u8, param: u8) {
        let ack = |reply: [u8; 3]| vec![0x10, 0xFF, sub, reg, reply[0], reply[1], reply[2]];
        match (sub, reg) {
            (0x80, 0x00) => self.answer(ack([0, 0, 0])),
            (0x81, 0x02) => {
                let count = self.slots.iter().flatten().count() as u8;
                self.answer(ack([0, count, 0]));
            }
            (0x80, 0x02) => {
                self.answer(ack([0, 0, 0]));
                if param == 0x02 {
                    let announced: Vec<_> = (1..=6u8)
                        .filter_map(|s| self.slots[s as usize].as_ref().map(|d| (s, d.wpid())))
                        .collect();
                    for (slot, wpid) in announced {
                        // 0x41 connection notification: link up, a mouse.
                        self.answer(vec![
                            0x10,
                            slot,
                            0x41,
                            0x04,
                            0x02,
                            wpid as u8,
                            (wpid >> 8) as u8,
                        ]);
                    }
                }
            }
            _ => self.answer(vec![0x10, 0xFF, 0x8F, sub, reg, ERR_INVALID_ADDRESS, 0]),
        }
    }

    /// A HID++ 2.0 request (feature index, function|software id).
    fn write_long(&mut self, w: &[u8]) {
        let (slot, feature, function_byte) = (w[1], w[2], w[3]);
        if slot == 0xFF {
            return;
        }
        let Some(SimDevice::Hidpp20 {
            name,
            battery_percent,
            flaky_first,
            ..
        }) = self.slots.get_mut(slot as usize).and_then(Option::as_mut)
        else {
            // Verified on a C547: the receiver answers for an empty slot with a
            // short error on the short collection.
            self.answer(vec![
                0x10,
                slot,
                0x8F,
                feature,
                function_byte,
                ERR_UNKNOWN_DEVICE,
                0,
            ]);
            return;
        };
        if *flaky_first > 0 {
            *flaky_first -= 1;
            return;
        }
        let reply = Self::feature_reply(
            slot,
            feature,
            function_byte,
            [w[4], w[5], w[6]],
            name,
            *battery_percent,
        );
        self.answer(reply);
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
            (Collection::Long, 0x11) => state.write_long(data),
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
                if queue.front().is_some_and(|(due, _)| *due <= Instant::now()) {
                    let (_, report) = queue.pop_front().unwrap();
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

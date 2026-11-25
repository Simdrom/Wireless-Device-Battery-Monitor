//! Rapoo VT7 Max (24AE:1416) and VT9PRO 4K (24AE:1211) mice on their 2.4 GHz receivers.
//!
//! Protocol as documented by TaskbarVitals (GPL-3.0, github.com/symNistaken/TaskbarVitals, docs/rapoo.md
//! and src/drivers/rapoo.cpp: Rapoo Hub's VT_nrf54L decoder and captures from real mice) and BatteryTray
//! (MIT, github.com/qu1ncky/BatteryTray, src/providers/devices/passive_mice.rs, which lists the VT7 Max);
//! implemented here from those facts. The mouse pushes its state on FF00:0002 by itself every few seconds;
//! **nothing is sent**:
//!
//!   07 MO .. .. .. .. .. ST LL ...     MO low nibble 0 receiver, 1 Bluetooth, 2 cable
//!                                      ST 1 on battery, 2 charging (0 = not valid yet), LL level
//!
//! The older firmware's 7-byte report on the same collection (VT3 Pro, VT1 Pro Max and, by OpenMouse's
//! captures, VT9 Pro on 24AE:1205; Device-Battery-Info's RapooProtocol.cs, github.com/PyFlat/
//! Device-Battery-Info), expected from the VT9PRO 4K's receiver, whose collection has that length (#11):
//!
//!   BB B0 .. .. .. ST LL               ST, LL as above; LL 7F = not known while charging
//!
//! The other vendor collections (settings, firmware) are never opened. A mouse that is off or asleep
//! pushes nothing: it is not listed until it does, then shown asleep by the app. Not tested on hardware by
//! this project (issues #249, #11).
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::device::{DeviceKind, DeviceStatus, PollContext};
use crate::hid::{HidColl, HidDev};
use crate::util::{Deadline, hex4};

const VID: u16 = 0x24AE;
/// Reports come every 3 to 10 s while the mouse is in use.
const WAIT_MS: u64 = 5500;
/// Nothing pushed: listen again only after this, so a sleeping mouse does not hold up every read.
const SILENT_EVERY: Duration = Duration::from_secs(2 * 60);

struct Model {
    pid: u16,
    name: &'static str,
    /// Input report length of its FF00:0002 collection.
    len: u16,
    decode: fn(&[u8]) -> Option<(i32, bool)>,
}

const MODELS: [Model; 2] = [
    Model { pid: 0x1416, name: "Rapoo VT7 Max", len: 19, decode: decode_nordic },
    Model { pid: 0x1211, name: "Rapoo VT9PRO 4K", len: 7, decode: decode_legacy },
];

/// State and level bytes of a status report.
fn state(st: u8, level: u8) -> Option<(i32, bool)> {
    (matches!(st, 1 | 2) && level <= 100).then_some((level as i32, st == 2))
}

/// Report 07 (report id included): level, charging.
pub fn decode_nordic(r: &[u8]) -> Option<(i32, bool)> {
    if r.len() < 9 || r[0] != 0x07 || r[1] & 0x0F > 2 {
        return None;
    }
    state(r[7], r[8])
}

/// The older firmware's report BB (report id included): level, charging.
pub fn decode_legacy(r: &[u8]) -> Option<(i32, bool)> {
    if r.len() < 7 || r[..2] != [0xBB, 0xB0] {
        return None;
    }
    state(r[5], r[6])
}

fn listen(c: &HidColl, decode: fn(&[u8]) -> Option<(i32, bool)>) -> Option<(i32, bool)> {
    let d = HidDev::open(c)?;
    let end = Deadline::in_ms(WAIT_MS);
    while !end.passed() {
        let started = Instant::now();
        let left = end.left_ms();
        match d.read(left) {
            Some(r) => {
                if let Some(v) = decode(&r) {
                    return Some(v);
                }
            }
            // An error returns at once (receiver unplugged); a timeout takes the full wait.
            None if started.elapsed().as_millis() + 20 < left as u128 => return None,
            None => {}
        }
    }
    None
}

static NEXT_TRY: Mutex<Option<HashMap<u16, Instant>>> = Mutex::new(None);

pub fn poll(ctx: &mut PollContext) {
    for m in &MODELS {
        if ctx.is_claimed(VID, m.pid) {
            continue;
        }
        let colls = ctx.find(VID, m.pid, |c| c.page() == 0xFF00 && c.usage() == 0x0002 && c.in_len() == m.len);
        if colls.is_empty() {
            continue;
        }
        let mut next = NEXT_TRY.lock().unwrap_or_else(|e| e.into_inner());
        let next = next.get_or_insert_with(HashMap::new);
        if next.get(&m.pid).is_some_and(|t| Instant::now() < *t) {
            continue;
        }
        let Some((percent, charging)) = colls.iter().find_map(|c| listen(c, m.decode)) else {
            next.insert(m.pid, Instant::now() + SILENT_EVERY);
            continue;
        };
        next.remove(&m.pid);
        let s = DeviceStatus {
            key: format!("rapoo:{}", hex4(m.pid)),
            name: m.name.into(),
            kind: DeviceKind::Mouse,
            via: "2.4 GHz receiver".into(),
            percent,
            charging,
            hw: Some((VID, m.pid)),
            grace_sec: 1800,
            ..Default::default()
        };
        ctx.add(s, VID, m.pid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nordic_reports() {
        // Captured by TaskbarVitals: VT3 Max Gen 2 on its receiver, 25 %; VT9 Gen 2 on the cable, 100 %.
        let rx = [0x07, 0x20, 0x03, 0x20, 0x03, 0x20, 0x03, 0x01, 0x19, 0x00, 0x04, 0x84, 0x00, 0x17, 0x46, 0x00, 0x02, 0x03, 0xBC];
        assert_eq!(decode_nordic(&rx), Some((25, false)));
        let cable = [0x07, 0x12, 0x04, 0x80, 0x0C, 0x80, 0x0C, 0x02, 0x64, 0x00, 0x03, 0x01, 0x00, 0x14, 0x46, 0x01, 0x02, 0x02, 0x00];
        assert_eq!(decode_nordic(&cable), Some((100, true)));
    }

    #[test]
    fn legacy_reports() {
        // A VT1 Pro Max owner's captures (TaskbarVitals): receiver 100 %, cable 95 % charging.
        assert_eq!(decode_legacy(&[0xBB, 0xB0, 0x71, 0x84, 0x03, 0x01, 0x64]), Some((100, false)));
        assert_eq!(decode_legacy(&[0xBB, 0xB0, 0x71, 0x84, 0x03, 0x02, 0x5F]), Some((95, true)));
        assert_eq!(decode_legacy(&[0xBB, 0xB0, 0x71, 0x84, 0x03, 0x02, 0x7F]), None);
        assert_eq!(decode_legacy(&[0xBB, 0xB1, 0x71, 0x84, 0x03, 0x01, 0x64]), None);
    }

    #[test]
    fn nordic_rejected() {
        assert_eq!(decode_nordic(&[0x07, 0x20, 0, 0, 0, 0, 0, 0x00, 0x19]), None); // not valid yet
        assert_eq!(decode_nordic(&[0x07, 0x20, 0, 0, 0, 0, 0, 0x01, 101]), None);
        assert_eq!(decode_nordic(&[0x07, 0x23, 0, 0, 0, 0, 0, 0x01, 50]), None);
        assert_eq!(decode_nordic(&[0x08, 0x20, 0, 0, 0, 0, 0, 0x01, 50]), None);
        assert_eq!(decode_nordic(&[0x07, 0x20]), None);
    }
}

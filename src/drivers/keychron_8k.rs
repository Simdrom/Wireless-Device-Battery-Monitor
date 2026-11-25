//! Keychron mice on the TurboLink 8K receiver (3434:D05A).
//!
//! Protocol as documented by BrimDeck (Apache-2.0, github.com/MoFeng2223/BrimDeck,
//! src/BrimDeck.Core/Brands/Keychron.cs: Keychron Launcher uses its "8k" protocol for any Keychron device
//! with an FFC1:0001 collection, the two receivers it excludes are not this one) and TaskbarVitals
//! (GPL-3.0, github.com/symNistaken/TaskbarVitals, src/drivers/keychron.cpp: the same queries, checked on
//! a real M6 on the Link-KM, 3434:D029); implemented here from those facts. Vendor collection FFC1:0001,
//! 64-byte reports:
//!
//!   output B5 03 ...  ->  input .. 03 ..   from byte 3, three slots of vid LE, pid LE, 1 = connected
//!   output B3 06 ...  ->  input B4 06 ..   byte 20: bits 0..6 level, bit 7 charging
//!
//! Only these two reads are sent. Not tested on hardware by this project (issue #45).
use crate::device::{DeviceKind, DeviceStatus, PollContext};
use crate::hid::{HidColl, HidDev};

const VID: u16 = 0x3434;
const PID: u16 = 0xD05A;
const REPLY_MS: u64 = 600;

/// Slots reply: a mouse is connected to the receiver.
pub fn parse_slots(r: &[u8]) -> Option<bool> {
    (r.len() >= 21 && r[0] != 0xB5 && r[1] == 0x03).then(|| [3, 8, 13].iter().any(|&i| (r[i] != 0 || r[i + 1] != 0) && r[i + 4] != 0))
}

/// Status reply: level, charging.
pub fn parse_status(r: &[u8]) -> Option<(i32, bool)> {
    let level = *r.get(20)? & 0x7F;
    (r[..2] == [0xB4, 0x06] && (1..=100).contains(&level)).then(|| (level as i32, r[20] & 0x80 != 0))
}

enum Reading {
    Battery(i32, bool),
    Away,
}

fn read(c: &HidColl) -> Option<Reading> {
    let d = HidDev::open(c)?;
    if !d.ask(&[0xB5, 0x03], REPLY_MS, parse_slots)? {
        return Some(Reading::Away);
    }
    let (level, charging) = d.ask(&[0xB3, 0x06], REPLY_MS, parse_status)?;
    Some(Reading::Battery(level, charging))
}

pub fn poll(ctx: &mut PollContext) {
    if ctx.is_claimed(VID, PID) {
        return;
    }
    let colls = ctx.find(VID, PID, |c| c.page() == 0xFFC1 && c.usage() == 0x0001 && c.in_len() >= 21 && c.out_len() >= 21);
    let Some(reading) = colls.iter().find_map(read) else { return };
    let base = DeviceStatus {
        key: "keychron:d05a".into(),
        name: "Keychron mouse".into(),
        kind: DeviceKind::Mouse,
        via: "2.4 GHz receiver".into(),
        hw: Some((VID, PID)),
        grace_sec: 1800,
        ..Default::default()
    };
    let s = match reading {
        Reading::Battery(percent, charging) => DeviceStatus { percent, charging, ..base },
        Reading::Away => DeviceStatus { connected: false, ..base },
    };
    ctx.add(s, VID, PID);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots() {
        let mut r = [0u8; 64];
        r[..2].copy_from_slice(&[0xB6, 0x03]);
        assert_eq!(parse_slots(&r), Some(false));
        r[8..13].copy_from_slice(&[0x34, 0x34, 0x64, 0xD0, 1]);
        assert_eq!(parse_slots(&r), Some(true));
        r[12] = 0;
        assert_eq!(parse_slots(&r), Some(false));
        r[0] = 0xB5; // the request read back
        assert_eq!(parse_slots(&r), None);
    }

    #[test]
    fn status() {
        let mut r = [0u8; 64];
        r[..2].copy_from_slice(&[0xB4, 0x06]);
        assert_eq!(parse_status(&r), None); // no level
        r[20] = 0x80 | 55;
        assert_eq!(parse_status(&r), Some((55, true)));
        r[20] = 90;
        assert_eq!(parse_status(&r), Some((90, false)));
        r[0] = 0xB3;
        assert_eq!(parse_status(&r), None);
        assert_eq!(parse_status(&r[..20]), None);
    }
}

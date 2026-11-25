//! BETOP (BEITONG) Kunpeng 70 controller, BTP-KP70A, on its X-input dongle (20BC:5130).
//!
//! Protocol as documented by BrimDeck (Apache-2.0, github.com/MoFeng2223/BrimDeck,
//! src/BrimDeck.Core/Brands/Betop.cs: BETOP's own client lists 5130 among the models it reads unscrambled)
//! and HaloBattery (MIT, github.com/HeyOkay/HaloBattery, providers/beitong.py, from a capture of that
//! client with a KP40D); implemented here from those facts. The dongle's vendor collection (usage page
//! 00FF or FF00, usage 0003), report 02:
//!
//!   request  02 15 00 ...
//!   reply    02 15 LL 00 ST      LL = level 1..100, ST = 5x, low nibble non-zero while charging
//!
//! Only this read is sent. Not tested on hardware by this project (issue #212).
use crate::device::{DeviceKind, DeviceStatus, PollContext};
use crate::hid::HidDev;

const VID: u16 = 0x20BC;
const PID: u16 = 0x5130;
const REQUEST: [u8; 2] = [0x02, 0x15];
const REPLY_MS: u64 = 800;

/// Level, charging. The request read back (all zeros after the command) is not a reply.
pub fn parse(r: &[u8]) -> Option<(i32, bool)> {
    (r.len() >= 5 && r[..2] == REQUEST && r[3] == 0 && r[4] & 0xF0 == 0x50 && (1..=100).contains(&r[2])).then(|| (r[2] as i32, r[4] & 0x0F != 0))
}

pub fn poll(ctx: &mut PollContext) {
    if ctx.is_claimed(VID, PID) {
        return;
    }
    let colls = ctx.find(VID, PID, |c| matches!(c.page(), 0x00FF | 0xFF00) && c.usage() == 0x0003 && c.in_len() >= 5 && c.out_len() >= 2);
    if colls.is_empty() {
        return;
    }
    let base = DeviceStatus {
        key: "betop:5130".into(),
        name: "BETOP BTP-KP70A".into(),
        kind: DeviceKind::Gamepad,
        via: "2.4 GHz dongle".into(),
        hw: Some((VID, PID)),
        grace_sec: 600,
        ..Default::default()
    };
    let s = match colls.iter().find_map(|c| HidDev::open(c)?.ask(&REQUEST, REPLY_MS, parse)) {
        Some((percent, charging)) => DeviceStatus { percent, charging, ..base },
        None => DeviceStatus { connected: false, ..base },
    };
    ctx.add(s, VID, PID);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies() {
        assert_eq!(parse(&[0x02, 0x15, 80, 0x00, 0x50]), Some((80, false)));
        assert_eq!(parse(&[0x02, 0x15, 35, 0x00, 0x51, 0]), Some((35, true)));
        assert_eq!(parse(&[0x02, 0x15, 0, 0, 0]), None); // the request read back
        assert_eq!(parse(&[0x02, 0x15, 0, 0x00, 0x50]), None); // not ready
        assert_eq!(parse(&[0x02, 0x15, 101, 0x00, 0x50]), None);
        assert_eq!(parse(&[0x02, 0x16, 80, 0x00, 0x50]), None);
    }
}

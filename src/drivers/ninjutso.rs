//! Ninjutso Sora V3 mouse on its 2.4 GHz receiver (093A:EB02).
//!
//! Protocol as documented by MouseDock (MIT, github.com/late-autumn-gif/mousedock,
//! VendorProtocol.MouseExpansionFeature.cs, "sora-v3") and TaskbarVitals (GPL-3.0,
//! github.com/symNistaken/TaskbarVitals, src/drivers/ninjutso.cpp: the same exchange, verified on a
//! Ninjutso Ten Air, 093A:EB01, from NinjaForce's web driver); implemented here from those facts. Feature
//! report 06 (16 bytes) on FF01:0001:
//!
//!   set feature  06 CC 00 00 01 00 ...     CC = 13 link, 12 level, 11 charging
//!   get feature  06 CC .. .. .. .. .. .. VV
//!
//! Only these three reads are sent. Not tested on hardware by this project (issue #198).
use crate::device::{DeviceKind, DeviceStatus, PollContext};
use crate::hid::{HidColl, HidDev};
use crate::util::sleep_ms;

const VID: u16 = 0x093A;
const PID: u16 = 0xEB02;
const REPORT: u8 = 0x06;
const LEN: usize = 16;
const CMD_CHARGING: u8 = 0x11;
const CMD_LEVEL: u8 = 0x12;
const CMD_LINK: u8 = 0x13;

pub fn request(cmd: u8) -> [u8; LEN] {
    let mut r = [0u8; LEN];
    r[..5].copy_from_slice(&[REPORT, cmd, 0, 0, 1]);
    r
}

/// The value in a feature report read back; the request itself is not a reply.
pub fn parse(r: &[u8], cmd: u8) -> Option<u8> {
    (r.len() >= LEN && r[..2] == [REPORT, cmd] && r[..LEN] != request(cmd)).then(|| r[8])
}

fn query(d: &HidDev, cmd: u8) -> Option<u8> {
    if !d.set_feature(&request(cmd)) {
        return None;
    }
    let mut raw = None;
    for _ in 0..3 {
        sleep_ms(30);
        raw = d.get_feature(REPORT).filter(|r| r.len() >= LEN && r[..2] == [REPORT, cmd]);
        if let Some(v) = raw.as_deref().and_then(|r| parse(r, cmd)) {
            return Some(v);
        }
    }
    // An answer of 0 may look like the request read back; it stands only for the flags.
    raw.filter(|_| cmd != CMD_LEVEL).map(|r| r[8])
}

enum Reading {
    Battery(i32, bool),
    Away,
}

fn read(c: &HidColl) -> Option<Reading> {
    let d = HidDev::open_feature_only(c)?;
    if query(&d, CMD_LINK)? != 1 {
        return Some(Reading::Away);
    }
    let level = query(&d, CMD_LEVEL).filter(|l| (1..=100).contains(l))?;
    Some(Reading::Battery(level as i32, query(&d, CMD_CHARGING) == Some(1)))
}

pub fn poll(ctx: &mut PollContext) {
    if ctx.is_claimed(VID, PID) {
        return;
    }
    let colls = ctx.find(VID, PID, |c| c.page() == 0xFF01 && c.usage() == 0x0001 && c.feat_len() as usize == LEN);
    let Some(reading) = colls.iter().find_map(read) else { return };
    let base = DeviceStatus {
        key: "ninjutso:eb02".into(),
        name: "Ninjutso Sora V3".into(),
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
    fn frames() {
        assert_eq!(request(CMD_LEVEL)[..6], [0x06, 0x12, 0, 0, 1, 0]);
        let mut r = request(CMD_LEVEL);
        assert_eq!(parse(&r, CMD_LEVEL), None); // the request read back
        r[8] = 73;
        assert_eq!(parse(&r, CMD_LEVEL), Some(73));
        assert_eq!(parse(&r, CMD_LINK), None);
        assert_eq!(parse(&r[..8], CMD_LEVEL), None);
    }
}

//! HyperX Cloud Stinger 2 Wireless headset over its 2.4 GHz dongle (VID 03F0, PID 0D93).
//!
//! Protocol as documented by HyperHeadset (MIT, github.com/LennardKittner/HyperHeadset,
//! src/devices/cloud_ii_wireless_dts.rs and old_cloud_2_doc/Headset_protocol.md, the Cloud II Wireless
//! DTS exchange this dongle shares), HeadsetControl (GPL-3.0, lib/devices/hyperx_cloud_2_wireless.hpp)
//! and BatteryTray (MIT, github.com/qu1ncky/BatteryTray, src/providers/headset.rs); implemented here
//! from those facts. Vendor collection FF90:0303:
//!
//!   request  06 FF BB 02 00 ...   battery    reply 06 FF BB 02 .. .. .. LL    LL = level %
//!   request  06 FF BB 03 00 ...   charging   reply 06 FF BB 03 SS             SS = 1 charging
//!
//! Only these two reads are sent. No reply means the headset is off (the dongle stays). Issue #203 did
//! not report the dongle's id; 0D93 is the one those projects list. Not tested on hardware here.
use crate::device::{DeviceKind, DeviceStatus, PollContext};
use crate::hid::{HidColl, HidDev};

const VID: u16 = 0x03F0;
const PID: u16 = 0x0D93;
const LEVEL: u8 = 0x02;
const CHARGING: u8 = 0x03;
const REPLY_MS: u64 = 1000;

pub fn request(cmd: u8) -> [u8; 4] {
    [0x06, 0xFF, 0xBB, cmd] // zero-padded to the report by HidDev::write
}

/// A reply to `cmd` (report id included).
pub fn parse(r: &[u8], cmd: u8) -> Option<u8> {
    if r.len() < 8 || r[..4] != request(cmd) {
        return None;
    }
    Some(if cmd == LEVEL { r[7] } else { r[4] })
}

fn read(c: &HidColl) -> Option<(i32, bool)> {
    let d = HidDev::open(c)?;
    let level = d.ask(&request(LEVEL), REPLY_MS, |r| parse(r, LEVEL)).filter(|&l| l <= 100)?;
    let charging = d.ask(&request(CHARGING), REPLY_MS, |r| parse(r, CHARGING)) == Some(1);
    Some((level as i32, charging))
}

pub fn poll(ctx: &mut PollContext) {
    if ctx.is_claimed(VID, PID) {
        return;
    }
    let colls = ctx.find(VID, PID, |c| c.page() == 0xFF90 && c.usage() == 0x0303 && c.in_len() >= 8 && c.out_len() >= 4);
    if colls.is_empty() {
        return;
    }
    let base = DeviceStatus {
        key: "hyperx:stinger2".into(),
        name: "HyperX Cloud Stinger 2 Wireless".into(),
        kind: DeviceKind::Headset,
        via: "2.4 GHz dongle".into(),
        hw: Some((VID, PID)),
        grace_sec: 600,
        ..Default::default()
    };
    let s = match colls.iter().find_map(read) {
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
        let mut r = [0u8; 20];
        r[..8].copy_from_slice(&[0x06, 0xFF, 0xBB, 0x02, 0, 0x0F, 0xA0, 87]);
        assert_eq!(parse(&r, LEVEL), Some(87));
        assert_eq!(parse(&r, CHARGING), None);
        r[..5].copy_from_slice(&[0x06, 0xFF, 0xBB, 0x03, 1]);
        assert_eq!(parse(&r, CHARGING), Some(1));
        r[2] = 0xBC;
        assert_eq!(parse(&r, CHARGING), None);
        assert_eq!(parse(&[0x06, 0xFF, 0xBB, 0x02], LEVEL), None);
    }
}

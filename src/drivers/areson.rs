//! Redragon (Areson / CompX) devices on the "2.4G Wireless Receiver" 25A7:FA07 and its alternate id
//! FA08: the M913 Impact Elite, the M915RGB-WL and others. Several devices use this receiver, so the
//! entry gets a generic name.
//!
//! Protocol as documented by the original C++ BatteryHub (GPL-3.0, src/drivers/areson.cpp and
//! docs/areson.md: captured from Redragon's software against an M915RGB-WL on 25A7:FA07, the level
//! matched the vendor app), m913-ctl (github.com/Qehbr/m913-ctl: 17-byte frames, feature / input
//! split, 0x55 checksum) and redragon-m913-battery-tray (github.com/Doomguy-BFG9000/
//! redragon-m913-battery-tray: GetPower and the 10 % scale); implemented here from those facts.
//! On interface 1 the request goes to the feature collection FF02:0002, the reply comes on the input
//! collection FF01:0000 (both 17 bytes):
//!
//!   request  set feature  08 04 00 ... 00 SS      GetPower; SS makes all 17 bytes sum to 0x55
//!   reply    input        09 04 00 .. .. 02 LL CC 00 ... SS
//!            LL = level 0..10 (x 10 %), CC non-zero = charging; all 17 bytes sum to 0x55
//!
//! Only this read is sent. No reply means the device is off or asleep (the receiver stays plugged in).
//!
//! The 25A7:FA7C receiver (Redragon M693, Fantech Helios Go XD5 and others; issue #225) answers the
//! same request with LL as a whole percentage: BatteryTray (MIT, github.com/qu1ncky/BatteryTray,
//! src/providers/oem_mouse.rs) and redragon-mouse-linux's PROTOCOL.md (github.com/rajchauhan28/
//! redragon-mouse-linux), as cited by TaskbarVitals' docs/areson.md. Not tested here on hardware.
use crate::device::{DeviceKind, DeviceStatus, PollContext};
use crate::hid::{self, HidColl, HidDev};
use crate::util::{hex4, sleep_ms};

const VID: u16 = 0x25A7;

struct Receiver {
    pid: u16,
    /// What one unit of the level byte is worth, in percent.
    step: i32,
    name: &'static str,
}

const RECEIVERS: [Receiver; 3] = [
    Receiver { pid: 0xFA07, step: 10, name: "Redragon wireless device" },
    Receiver { pid: 0xFA08, step: 10, name: "Redragon wireless device" },
    Receiver { pid: 0xFA7C, step: 1, name: "Wireless mouse" },
];

const LEN: usize = 17;
const WAIT_MS: u32 = 1800;

fn sum(b: &[u8]) -> u8 {
    b.iter().fold(0u8, |a, &x| a.wrapping_add(x))
}

pub fn request() -> [u8; LEN] {
    let mut r = [0u8; LEN];
    r[0] = 0x08;
    r[1] = 0x04;
    r[LEN - 1] = 0x55u8.wrapping_sub(sum(&r[..LEN - 1]));
    r
}

/// GetPower reply: Some((percent, charging)); `step` is what one unit of the level is worth.
pub fn decode(r: &[u8], step: i32) -> Option<(i32, bool)> {
    if r.len() < LEN || sum(&r[..LEN]) != 0x55 || r[0] != 0x09 || r[1] != 0x04 || r[2] != 0x00 {
        return None;
    }
    let percent = r[6] as i32 * step;
    (percent <= 100).then_some((percent, r[7] != 0))
}

fn query(cmd: &HidColl, input: &HidColl, step: i32) -> Option<(i32, bool)> {
    let c = HidDev::open_feature_only(cmd).or_else(|| HidDev::open(cmd))?;
    let rx = HidDev::open(input)?;
    for attempt in 0..2 {
        if attempt > 0 {
            sleep_ms(200);
        }
        rx.flush(); // a report queued before the request would be taken for the answer
        if !c.set_feature(&request()) {
            return None;
        }
        if let Some(v) = rx.read(WAIT_MS).and_then(|r| decode(&r, step)) {
            return Some(v);
        }
    }
    None
}

pub fn poll(ctx: &mut PollContext) {
    for rx in &RECEIVERS {
        let pid = rx.pid;
        if ctx.is_claimed(VID, pid) {
            continue;
        }
        let cmds = ctx.find(VID, pid, |c| c.iface == 1 && c.page() == 0xFF02 && c.usage() == 0x0002 && c.feat_len() as usize >= LEN);
        let inputs = ctx.find(VID, pid, |c| c.iface == 1 && c.page() == 0xFF01 && c.usage() == 0x0000 && c.in_len() as usize >= LEN);
        let (Some(cmd), Some(input)) = (cmds.first(), inputs.first()) else { continue };
        let kind = hid::kind_from_collections(&ctx.colls, VID, pid);
        let base = DeviceStatus {
            key: format!("areson:{}:{}", hex4(VID), hex4(pid)),
            name: rx.name.into(),
            kind: if kind == DeviceKind::Other { DeviceKind::Mouse } else { kind },
            via: "2.4 GHz receiver".into(),
            hw: Some((VID, pid)),
            grace_sec: 1800,
            ..Default::default()
        };
        let s = match query(cmd, input, rx.step) {
            Some((percent, charging)) => DeviceStatus { percent, charging, ..base },
            None => DeviceStatus { connected: false, ..base },
        };
        ctx.add(s, VID, pid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_request() {
        // docs/areson.md: 08 04 00 ... 00 49
        let r = request();
        assert_eq!(r[..2], [0x08, 0x04]);
        assert_eq!(r[16], 0x49);
    }

    #[test]
    fn captured_reply() {
        // docs/areson.md: 09 04 00 00 00 02 08 00 ... 3E = 80 %, not charging.
        let mut r = [0u8; 17];
        r[..8].copy_from_slice(&[0x09, 0x04, 0x00, 0x00, 0x00, 0x02, 0x08, 0x00]);
        r[16] = 0x3E;
        assert_eq!(decode(&r, 10), Some((80, false)));
    }

    #[test]
    fn charging_and_rejects() {
        let frame = |level: u8, chg: u8| {
            let mut r = [0u8; 17];
            r[..8].copy_from_slice(&[0x09, 0x04, 0x00, 0x00, 0x00, 0x02, level, chg]);
            r[16] = 0x55u8.wrapping_sub(sum(&r[..16]));
            r
        };
        assert_eq!(decode(&frame(5, 1), 10), Some((50, true)));
        assert_eq!(decode(&frame(11, 0), 10), None); // not on the 0..10 scale
        let mut bad = frame(8, 0);
        bad[16] ^= 1;
        assert_eq!(decode(&bad, 10), None); // checksum
        assert_eq!(decode(&request(), 10), None); // our own request
        let mut status = frame(1, 0);
        status[1] = 0x03; // GetConnectStatus reply
        status[16] = 0x55u8.wrapping_sub(sum(&status[..16]));
        assert_eq!(decode(&status, 10), None);
    }

    #[test]
    fn whole_percent_receiver() {
        let mut r = [0u8; LEN];
        r[..8].copy_from_slice(&[0x09, 0x04, 0x00, 0x00, 0x00, 0x02, 87, 0x01]);
        r[LEN - 1] = 0x55u8.wrapping_sub(sum(&r[..LEN - 1]));
        assert_eq!(decode(&r, 1), Some((87, true)));
        assert_eq!(decode(&r, 10), None); // 870 % on the tens scale
    }
}

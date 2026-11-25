//! SONiX keyboards on their 2.4 GHz dongle: AULA F108 Pro and AJAZZ AK820 MAX (05AC:024F, Apple's
//! vendor id reused), AJAZZ AK980 V2 (0C45:FEFE), AJAZZ AK820 Pro (0C45:FDFD or 0C45:FEFE).
//!
//! Protocol as documented by sonix-keyboard-studio (github.com/donglover69/sonix-keyboard-studio,
//! README "2.4G dongles"), ak820-rs (github.com/deddroid/ak820-rs) and ak820-studio
//! (github.com/usharik/ak820-studio), as used by the original C++ BatteryHub (GPL-3.0,
//! src/drivers/sonix.cpp, tested on an AK820 MAX and an EPOMAKER EA75); implemented here from those
//! facts. The dongle's vendor collection on interface 3 (usage page FF60, usage 0061, 33-byte input
//! and output reports: report id 0 + 32 bytes):
//!
//!   request  20 01 00 ... SS      SS (byte 31) = sum of bytes 0..30
//!   reply    20 01 00 LL ...      LL = level in percent; 0 = keyboard off, out of range or not in 2.4G mode
//!
//! The reply is recognised by its echoed first three bytes. Read-only; no charging indication. Other
//! keyboards on 05AC:024F (no FF60 collection on interface 3) are not touched.
//!
//! Dongles that TaskbarVitals (GPL-3.0, github.com/symNistaken/TaskbarVitals, src/drivers/sonix.cpp)
//! lists for the same request, not confirmed on hardware there or here: 342D:E411 (#122).
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::device::{DeviceKind, DeviceStatus, PollContext};
use crate::hid::{self, HidColl, HidDev};
use crate::util::hex4;

struct Dongle {
    vid: u16,
    pid: u16,
    iface: i32,
    /// Seen answering on hardware. The others share FF60:0061 with plain VIA firmware, which may
    /// echo the request: an echo is no reading there, and a silent one is asked only now and then.
    confirmed: bool,
}

const DONGLES: [Dongle; 4] = [
    Dongle { vid: 0x05AC, pid: 0x024F, iface: 3, confirmed: true },
    Dongle { vid: 0x0C45, pid: 0xFEFE, iface: 3, confirmed: true },
    Dongle { vid: 0x0C45, pid: 0xFDFD, iface: 3, confirmed: true },
    // Dark Project ALU87A Onionite (#122)
    Dongle { vid: 0x342D, pid: 0xE411, iface: 1, confirmed: false },
];
const SILENT_QUERY_EVERY: Duration = Duration::from_secs(10 * 60);
const LEN: usize = 32;
const REPLY_MS: u64 = 500;

/// The output report, report id 0 first.
pub fn request() -> [u8; LEN + 1] {
    let mut r = [0u8; LEN + 1];
    r[1] = 0x20;
    r[2] = 0x01;
    r[LEN] = r[1..LEN].iter().fold(0u8, |a, &b| a.wrapping_add(b));
    r
}

/// The request read back (VIA firmware without this command may echo it).
pub fn is_echo(r: &[u8]) -> bool {
    r == &request()[1..]
}

/// The level byte of a reply (report id 0 already stripped by the reader).
pub fn parse(r: &[u8]) -> Option<u8> {
    (r.len() >= 4 && r[..3] == [0x20, 0x01, 0x00]).then(|| r[3])
}

fn query(d: &HidDev, confirmed: bool) -> Option<u8> {
    d.ask(&request(), REPLY_MS, |r| if !confirmed && is_echo(r) { None } else { parse(r) })
}

fn read(c: &HidColl, confirmed: bool) -> Option<u8> {
    let d = HidDev::open(c)?;
    // The first request can go unanswered while the 2.4 GHz link wakes up.
    query(&d, confirmed).or_else(|| query(&d, confirmed))
}

static NEXT_TRY: Mutex<Option<HashMap<(u16, u16), Instant>>> = Mutex::new(None);

fn name(c: &HidColl) -> String {
    let n = hid::product_string(c);
    let n = n.trim().trim_end_matches(" 2.4G").trim_end_matches("_2.4G").trim();
    if n.is_empty() || n.eq_ignore_ascii_case("usb dongle") || n.to_ascii_lowercase().ends_with("dongle") { "Wireless keyboard".into() } else { n.to_string() }
}

pub fn poll(ctx: &mut PollContext) {
    for m in &DONGLES {
        let (vid, pid, confirmed) = (m.vid, m.pid, m.confirmed);
        if ctx.is_claimed(vid, pid) {
            continue;
        }
        let colls = ctx.find(vid, pid, |c| {
            c.iface == m.iface && c.page() == 0xFF60 && c.usage() == 0x0061 && c.out_len() as usize == LEN + 1 && c.in_len() as usize == LEN + 1
        });
        if colls.is_empty() {
            continue;
        }
        let mut next = NEXT_TRY.lock().unwrap_or_else(|e| e.into_inner());
        let next = next.get_or_insert_with(HashMap::new);
        if next.get(&(vid, pid)).is_some_and(|t| Instant::now() < *t) {
            continue;
        }
        let Some((c, level)) = colls.iter().find_map(|c| read(c, confirmed).map(|l| (c, l))) else {
            if !confirmed {
                next.insert((vid, pid), Instant::now() + SILENT_QUERY_EVERY);
            }
            continue;
        };
        next.remove(&(vid, pid));
        let base = DeviceStatus {
            key: format!("sonix:{}:{}", hex4(vid), hex4(pid)),
            name: name(c),
            kind: DeviceKind::Keyboard,
            via: "2.4 GHz receiver".into(),
            hw: Some((vid, pid)),
            grace_sec: 1800,
            ..Default::default()
        };
        let s = if (1..=100).contains(&level) { DeviceStatus { percent: level as i32, ..base } } else { DeviceStatus { connected: false, ..base } };
        ctx.add(s, vid, pid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_frame() {
        let r = request();
        assert_eq!(r.len(), 33);
        assert_eq!(&r[..3], &[0x00, 0x20, 0x01]);
        assert_eq!(r[32], 0x21);
        assert!(r[3..32].iter().all(|&b| b == 0));
    }

    #[test]
    fn echo() {
        assert!(is_echo(&request()[1..]));
        let mut r = request()[1..].to_vec();
        r[3] = 73;
        assert!(!is_echo(&r));
        assert_eq!(parse(&r), Some(73));
    }

    #[test]
    fn replies() {
        let mut r = vec![0u8; 32];
        r[..4].copy_from_slice(&[0x20, 0x01, 0x00, 87]);
        assert_eq!(parse(&r), Some(87));
        r[3] = 0;
        assert_eq!(parse(&r), Some(0)); // keyboard off / not in 2.4G mode
        assert_eq!(parse(&[0x20, 0x02, 0x00, 50]), None);
        assert_eq!(parse(&[0x21, 0x01, 0x00, 50]), None);
        assert_eq!(parse(&[0x20, 0x01, 0x00]), None);
    }
}

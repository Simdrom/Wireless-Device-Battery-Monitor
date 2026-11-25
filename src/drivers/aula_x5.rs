//! AULA Kunlun X5 mouse on its 2.4 GHz receiver (38A6:28F9).
//!
//! Protocol as documented by MouseDock (MIT, github.com/late-autumn-gif/mousedock,
//! VendorProtocol.MouseExpansionAula.cs, which lists this receiver) and BrimDeck (Apache-2.0,
//! github.com/MoFeng2223/BrimDeck, src/BrimDeck.Core/Brands/Aula.cs: the status read on AULA's L9 Ultra
//! receiver, 38A6:28FD); implemented here from those facts. Vendor collection FFA8:0002, 64 bytes each
//! way, output report 0B, input report 0A (or 0B):
//!
//!   request  0B D0 00 ...   link     reply 0A D0 .. .. LK             LK = 1 mouse linked
//!   request  0B 81 00 ...   status   reply 0A 81 .. 00 ... CH LL FL   byte 17 = 1 charging, 18 = level
//!
//! Only these two reads are sent. Not tested on hardware by this project (issue #194).
use crate::device::{DeviceKind, DeviceStatus, PollContext};
use crate::hid::{HidColl, HidDev};

const VID: u16 = 0x38A6;
const PID: u16 = 0x28F9;
const OUT: u8 = 0x0B;
const CMD_LINK: u8 = 0xD0;
const CMD_STATUS: u8 = 0x81;
const LEN: u16 = 64;
const REPLY_MS: u64 = 800;

fn is_reply(r: &[u8], cmd: u8) -> bool {
    // The request read back is all zeros after the command.
    r.len() >= 20 && matches!(r[0], 0x0A | 0x0B) && r[1] == cmd && r[2..].iter().any(|&b| b != 0)
}

/// Link reply: mouse linked.
pub fn parse_link(r: &[u8]) -> Option<bool> {
    is_reply(r, CMD_LINK).then(|| r[4] == 1)
}

/// Status reply: level, charging.
pub fn parse_status(r: &[u8]) -> Option<(i32, bool)> {
    (is_reply(r, CMD_STATUS) && r[3] == 0 && r[17] <= 1 && (1..=100).contains(&r[18]) && r[19] <= 1).then(|| (r[18] as i32, r[17] == 1))
}

enum Reading {
    Battery(i32, bool),
    Away,
}

fn read(c: &HidColl) -> Option<Reading> {
    let d = HidDev::open(c)?;
    if !d.ask(&[OUT, CMD_LINK], REPLY_MS, parse_link)? {
        return Some(Reading::Away);
    }
    let (level, charging) = d.ask(&[OUT, CMD_STATUS], REPLY_MS, parse_status)?;
    Some(Reading::Battery(level, charging))
}

pub fn poll(ctx: &mut PollContext) {
    if ctx.is_claimed(VID, PID) {
        return;
    }
    let colls = ctx.find(VID, PID, |c| c.page() == 0xFFA8 && c.usage() == 0x0002 && c.in_len() == LEN && c.out_len() == LEN);
    let Some(reading) = colls.iter().find_map(read) else { return };
    let base = DeviceStatus {
        key: "aula:38a6:28f9".into(),
        name: "AULA Kunlun X5".into(),
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

    fn frame(cmd: u8, at: &[(usize, u8)]) -> Vec<u8> {
        let mut r = vec![0u8; LEN as usize];
        r[..2].copy_from_slice(&[0x0A, cmd]);
        at.iter().for_each(|&(i, b)| r[i] = b);
        r
    }

    #[test]
    fn replies() {
        assert_eq!(parse_link(&frame(CMD_LINK, &[(4, 1)])), Some(true));
        assert_eq!(parse_link(&frame(CMD_LINK, &[(5, 1)])), Some(false));
        assert_eq!(parse_status(&frame(CMD_STATUS, &[(18, 66)])), Some((66, false)));
        assert_eq!(parse_status(&frame(CMD_STATUS, &[(17, 1), (18, 40)])), Some((40, true)));
    }

    #[test]
    fn rejected() {
        let mut echo = frame(CMD_STATUS, &[]);
        echo[0] = OUT;
        assert_eq!(parse_status(&echo), None);
        assert_eq!(parse_status(&frame(CMD_STATUS, &[(18, 101)])), None);
        assert_eq!(parse_status(&frame(CMD_STATUS, &[(3, 1), (18, 50)])), None); // error status
        assert_eq!(parse_status(&frame(CMD_LINK, &[(18, 50)])), None);
        assert_eq!(parse_link(&[0x0A, CMD_LINK]), None);
    }
}

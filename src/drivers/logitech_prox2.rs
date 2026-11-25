//! Logitech G PRO X 2 LIGHTSPEED headset (VID 046D, PID 0AF7) over its 2.4 GHz receiver.
//!
//! Not HID++ on FF43 (logitech.rs): Logitech's "Centurion" transport on the vendor collection
//! FFA0:0001 (interface 3), 64-byte output / input report 51. Protocol as documented by HeadsetControl
//! (GPL-3.0, github.com/Sapd/HeadsetControl, lib/devices/logitech_gpro_x2_lightspeed.hpp and
//! lib/devices/protocols/logitech_centurion_protocol.hpp), Solaar (GPL-2.0, its PRO X 2 0AF7 device
//! dump) and HaloBattery (MIT, github.com/HeyOkay/HaloBattery, providers/logitech_centurion.py,
//! confirmed on hardware in HaloBattery issue #103); implemented here from those facts.
//!
//!   frame     51 LL 00 <payload>                LL = payload length + 1
//!   direct    payload <feature index> <function | 01> <params>; the reply echoes the index, data at 2
//!   bridge    payload <bridge> 11 <size hi> <size lo> 00 <headset feature index> <function | 01> <params>
//!             the receiver acknowledges (<bridge> 11), the headset answers
//!             <bridge> 10 .. .. 00 <index> <function> <data>   (<index> FF = rejected)
//!   event     <bridge> 00 00 CC                 headset connected (CC 1) / disconnected (CC 0)
//!
//! Read: Root (index 0) function 0 with feature id 0003 gives the receiver's bridge index; through the
//! bridge, Root function 0 with 0104 gives the headset's battery feature index; its function 0 answers
//! [level %] [..] [state], state 1 / 2 = charging, 3 = charged. Firmware without 0104 gets
//! HeadsetControl's fixed request 51 08 00 03 1A 00 03 00 04 0A: 51 03 = acknowledgement, 51 05 with
//! byte 6 = 00 = headset off, 51 0B with byte 8 = 04 = battery (byte 10 level, byte 12 = 02 charging).
//! Only these reads are sent; no setting is changed.
//!
//! G325 LIGHTSPEED (046D:0B21, issue #206): the later Centurion headsets start their frames with 50 23
//! instead of 51 (HeadsetControl's logitech_g522_lightspeed.hpp, BatteryTray's centurion.rs), and Solaar
//! (lib/logitech_receiver/device.py) treats the G325 as a Centurion headset. Its receiver has the same
//! FFA0:0001 64/64 collection. A receiver ignores frames with another prefix, and the features are
//! looked up as above; no fixed request is sent to it. Not tested on hardware by this project.
use std::collections::HashMap;
use std::sync::Mutex;

use crate::device::{DeviceKind, DeviceStatus, PollContext};
use crate::hid::{HidColl, HidDev};
use crate::util::{Deadline, hex4};

const VID: u16 = 0x046D;
/// What starts every frame: report 51 on the PRO X 2, report 50 and the address 23 on later models.
const PRO_X_2: &[u8] = &[0x51];
const ADDRESSED: &[u8] = &[0x50, 0x23];

struct Model {
    pid: u16,
    name: &'static str,
    prefix: &'static [u8],
}

const MODELS: [Model; 2] = [
    Model { pid: 0x0AF7, name: "Logitech G PRO X 2 LIGHTSPEED", prefix: PRO_X_2 },
    Model { pid: 0x0B21, name: "Logitech G325 LIGHTSPEED", prefix: ADDRESSED },
];

const REPORT: u8 = 0x51;
const SW: u8 = 0x01;
const BRIDGE_SEND: u8 = 0x10;
const FEATURE_BRIDGE: u16 = 0x0003;
const FEATURE_BATTERY: u16 = 0x0104;
const LEGACY: [u8; 10] = [0x51, 0x08, 0x00, 0x03, 0x1A, 0x00, 0x03, 0x00, 0x04, 0x0A];
const WAIT_MS: u64 = 1500;

pub fn frame(prefix: &[u8], payload: &[u8]) -> [u8; 64] {
    let mut f = [0u8; 64];
    let n = prefix.len();
    f[..n].copy_from_slice(prefix);
    f[n] = payload.len() as u8 + 1;
    let len = payload.len().min(62 - n);
    f[n + 2..n + 2 + len].copy_from_slice(&payload[..len]);
    f
}

/// The payload of a received frame (after the length and flags bytes).
pub fn payload<'a>(prefix: &[u8], r: &'a [u8]) -> Option<&'a [u8]> {
    let r = r.strip_prefix(prefix)?;
    let len = *r.first()? as usize;
    (len > 1 && len < r.len()).then(|| &r[2..len + 1])
}

pub fn direct(prefix: &[u8], index: u8, function: u8, params: &[u8]) -> [u8; 64] {
    let mut p = vec![index, (function & 0xF0) | SW];
    p.extend_from_slice(params);
    frame(prefix, &p)
}

pub fn bridged(prefix: &[u8], bridge: u8, index: u8, function: u8, params: &[u8]) -> [u8; 64] {
    let size = 3 + params.len();
    let mut p = vec![bridge, BRIDGE_SEND | SW, (size >> 8) as u8 & 0x0F, size as u8, 0x00, index, (function & 0xF0) | SW];
    p.extend_from_slice(params);
    frame(prefix, &p)
}

/// Data of a direct reply for feature `index`.
pub fn direct_reply(p: &[u8], index: u8) -> Option<Vec<u8>> {
    (p.len() >= 2 && p[0] == index).then(|| p[2..].to_vec())
}

#[derive(Debug, PartialEq, Eq)]
pub enum Bridge {
    Ack,
    Answer(Vec<u8>),
    Rejected,
    Disconnected,
    Other,
}

/// Classifies one received payload while waiting for the headset's answer for its feature `index`.
pub fn bridge_reply(p: &[u8], bridge: u8, index: u8) -> Bridge {
    if p.len() >= 4 && p[0] == 0xFF && p[1] == bridge && p[2] == (BRIDGE_SEND | SW) {
        return Bridge::Rejected;
    }
    if p.len() < 2 || p[0] != bridge {
        return Bridge::Other;
    }
    if p[1] == 0x00 {
        return if p.len() >= 4 && p[3] == 0x00 { Bridge::Disconnected } else { Bridge::Other };
    }
    if p[1] >> 4 != BRIDGE_SEND >> 4 {
        return Bridge::Other;
    }
    match p[1] & 0x0F {
        SW => return Bridge::Ack,
        0 => {}
        _ => return Bridge::Other,
    }
    if p.len() < 7 || p[4] != 0x00 {
        return Bridge::Other;
    }
    if p[5] == index {
        Bridge::Answer(p[7..].to_vec())
    } else if p[5] == 0xFF && p[6] == index {
        Bridge::Rejected
    } else {
        Bridge::Other
    }
}

/// Battery feature data: Some((level, charging, charged)).
pub fn parse_battery(data: &[u8]) -> Option<(i32, bool, bool)> {
    let level = *data.first()?;
    let state = data.get(2).copied().unwrap_or(0);
    (level <= 100).then_some((level as i32, state == 1 || state == 2, state == 3))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Legacy {
    Battery(i32, bool),
    Off,
    Skip,
}

pub fn legacy_reply(r: &[u8]) -> Option<Legacy> {
    if r.len() < 2 || r[0] != REPORT {
        return None;
    }
    match r[1] {
        0x03 => Some(Legacy::Skip),
        0x05 if r.len() >= 7 && r[6] == 0x00 => Some(Legacy::Off),
        0x05 => Some(Legacy::Skip),
        0x0B if r.len() >= 13 && r[8] == 0x04 => (r[10] <= 100).then(|| Legacy::Battery(r[10] as i32, r[12] == 0x02)),
        _ => None,
    }
}

fn feature_params(id: u16) -> [u8; 2] {
    id.to_be_bytes()
}

enum Error {
    /// The headset is off or the request was rejected: show the receiver as disconnected.
    Offline,
    /// The headset has no such feature.
    NotSupported,
    /// No usable answer.
    Failed,
}

fn ask_direct(d: &HidDev, prefix: &[u8], index: u8, function: u8, params: &[u8]) -> Result<Vec<u8>, Error> {
    d.flush();
    if !d.write(&direct(prefix, index, function, params)) {
        return Err(Error::Failed);
    }
    let end = Deadline::in_ms(WAIT_MS);
    while !end.passed() {
        let Some(r) = d.read(end.left_ms()) else { continue };
        if let Some(data) = payload(prefix, &r).and_then(|p| direct_reply(p, index)) {
            return Ok(data);
        }
    }
    Err(Error::Failed)
}

fn ask_bridged(d: &HidDev, prefix: &[u8], bridge: u8, index: u8, function: u8, params: &[u8]) -> Result<Vec<u8>, Error> {
    d.flush();
    if !d.write(&bridged(prefix, bridge, index, function, params)) {
        return Err(Error::Failed);
    }
    let end = Deadline::in_ms(WAIT_MS);
    let mut acked = false;
    while !end.passed() {
        let Some(r) = d.read(end.left_ms()) else { continue };
        match payload(prefix, &r).map(|p| bridge_reply(p, bridge, index)) {
            Some(Bridge::Answer(data)) => return Ok(data),
            Some(Bridge::Ack) => acked = true,
            Some(Bridge::Rejected) | Some(Bridge::Disconnected) => return Err(Error::Offline),
            _ => {}
        }
    }
    // Acknowledged by the receiver but never answered by the headset: off or out of range.
    Err(if acked { Error::Offline } else { Error::Failed })
}

fn ask_legacy(d: &HidDev) -> Result<(i32, bool, bool), Error> {
    d.flush();
    if !d.write(&LEGACY) {
        return Err(Error::Failed);
    }
    let end = Deadline::in_ms(WAIT_MS);
    let mut reads = 0;
    while !end.passed() && reads < 4 {
        let Some(r) = d.read(end.left_ms()) else { continue };
        match legacy_reply(&r) {
            Some(Legacy::Battery(level, charging)) => return Ok((level, charging, false)),
            Some(Legacy::Off) => return Err(Error::Offline),
            Some(Legacy::Skip) => reads += 1,
            None => {}
        }
    }
    Err(Error::Failed)
}

/// Bridge and headset battery feature indexes, kept while they keep working.
type Indexes = HashMap<String, (u8, Option<u8>)>;
static INDEXES: Mutex<Option<Indexes>> = Mutex::new(None);

fn read(c: &HidColl, prefix: &[u8]) -> Result<(i32, bool, bool), Error> {
    let d = HidDev::open(c).ok_or(Error::Failed)?;
    let cached = INDEXES.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(&c.path).copied());
    let bridge = match cached {
        Some((b, _)) => b,
        None => match ask_direct(&d, prefix, 0x00, 0x00, &feature_params(FEATURE_BRIDGE))?.first() {
            Some(&b) if b != 0 => b,
            _ => return Err(Error::Failed),
        },
    };
    let battery = match cached.and_then(|(_, i)| i) {
        Some(i) => Ok(i),
        None => match ask_bridged(&d, prefix, bridge, 0x00, 0x00, &feature_params(FEATURE_BATTERY)) {
            Ok(data) => match data.first() {
                Some(&i) if i != 0 => Ok(i),
                _ => Err(Error::NotSupported),
            },
            Err(e) => Err(e),
        },
    };
    let remember = |battery: Option<u8>| {
        let mut g = INDEXES.lock().unwrap_or_else(|e| e.into_inner());
        g.get_or_insert_with(HashMap::new).insert(c.path.clone(), (bridge, battery));
    };
    match battery {
        Ok(index) => match ask_bridged(&d, prefix, bridge, index, 0x00, &[]) {
            Ok(data) => {
                remember(Some(index));
                parse_battery(&data).ok_or(Error::Failed)
            }
            Err(e) => {
                remember(None); // look the feature up again next time
                Err(e)
            }
        },
        Err(Error::NotSupported) => {
            remember(None);
            if prefix != PRO_X_2 {
                return Err(Error::Failed);
            }
            ask_legacy(&d)
        }
        Err(e) => {
            remember(None);
            Err(e)
        }
    }
}

pub fn poll(ctx: &mut PollContext) {
    for m in &MODELS {
        if ctx.is_claimed(VID, m.pid) {
            continue;
        }
        let colls = ctx.find(VID, m.pid, |c| c.page() == 0xFFA0 && c.usage() == 0x0001 && c.in_len() >= 64 && c.out_len() >= 64);
        if colls.is_empty() {
            continue;
        }
        let base = DeviceStatus {
            key: format!("logi-centurion:{}", hex4(m.pid)),
            name: m.name.into(),
            kind: DeviceKind::Headset,
            via: "2.4 GHz receiver".into(),
            hw: Some((VID, m.pid)),
            grace_sec: 60,
            ..Default::default()
        };
        let s = match colls.iter().find_map(|c| read(c, m.prefix).ok()) {
            Some((level, charging, full)) => DeviceStatus { percent: level, charging, full, ..base },
            None => DeviceStatus { connected: false, ..base },
        };
        ctx.add(s, VID, m.pid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rx(p: &[u8]) -> [u8; 64] {
        frame(PRO_X_2, p)
    }

    #[test]
    fn addressed_frames() {
        // HeadsetControl's G522: 50 23, then length, flags and the payload.
        assert_eq!(direct(ADDRESSED, 0, 0, &[0x00, 0x03])[..8], [0x50, 0x23, 0x05, 0x00, 0x00, 0x01, 0x00, 0x03]);
        let r = frame(ADDRESSED, &[0x00, 0x01, 0x03, 0x00, 0x00]);
        assert_eq!(payload(ADDRESSED, &r).and_then(|p| direct_reply(p, 0)), Some(vec![0x03, 0x00, 0x00]));
        assert_eq!(payload(PRO_X_2, &r), None);
        assert_eq!(payload(ADDRESSED, &[0x50, 0x23, 0x40, 0x00]), None);
    }

    #[test]
    fn frames() {
        // Root.getFeature(0003): 51 05 00 00 01 00 03
        assert_eq!(direct(PRO_X_2, 0, 0, &[0x00, 0x03])[..7], [0x51, 0x05, 0x00, 0x00, 0x01, 0x00, 0x03]);
        // Through bridge 3, headset Root.getFeature(0104): sub-message 00 00 01 01 04 (5 bytes).
        assert_eq!(bridged(PRO_X_2, 3, 0, 0, &[0x01, 0x04])[..12], [0x51, 0x0A, 0x00, 0x03, 0x11, 0x00, 0x05, 0x00, 0x00, 0x01, 0x01, 0x04]);
        assert_eq!(payload(PRO_X_2, &rx(&[0x00, 0x01, 0x03, 0x00, 0x00])), Some(&[0x00, 0x01, 0x03, 0x00, 0x00][..]));
        assert_eq!(payload(PRO_X_2, &[0x50, 0x02, 0x00, 0x00]), None);
    }

    #[test]
    fn direct_replies() {
        // Root answers the bridge index (3), as in Solaar's dump of a PRO X 2.
        let r = rx(&[0x00, 0x01, 0x03, 0x00, 0x00]);
        assert_eq!(payload(PRO_X_2, &r).and_then(|p| direct_reply(p, 0)), Some(vec![0x03, 0x00, 0x00]));
    }

    #[test]
    fn bridge_replies() {
        assert_eq!(bridge_reply(&[0x03, 0x11, 0x00, 0x05], 3, 4), Bridge::Ack);
        // Battery feature 4: 75 %, discharging.
        let answer = [0x03, 0x10, 0x00, 0x06, 0x00, 0x04, 0x01, 75, 0x00, 0x00];
        assert_eq!(bridge_reply(&answer, 3, 4), Bridge::Answer(vec![75, 0x00, 0x00]));
        assert_eq!(parse_battery(&[75, 0x00, 0x00]), Some((75, false, false)));
        assert_eq!(parse_battery(&[60, 0x00, 0x01]), Some((60, true, false)));
        assert_eq!(parse_battery(&[100, 0x00, 0x03]), Some((100, false, true)));
        assert_eq!(parse_battery(&[101]), None);
        assert_eq!(bridge_reply(&[0x03, 0x10, 0x00, 0x04, 0x00, 0xFF, 0x04, 0x05], 3, 4), Bridge::Rejected);
        assert_eq!(bridge_reply(&[0xFF, 0x03, 0x11, 0x05], 3, 4), Bridge::Rejected);
        assert_eq!(bridge_reply(&[0x03, 0x00, 0x00, 0x00], 3, 4), Bridge::Disconnected);
        assert_eq!(bridge_reply(&[0x03, 0x00, 0x00, 0x01], 3, 4), Bridge::Other);
        assert_eq!(bridge_reply(&[0x03, 0x10, 0x00, 0x06, 0x00, 0x02, 0x01, 75], 3, 4), Bridge::Other);
    }

    #[test]
    fn legacy() {
        assert_eq!(LEGACY, [0x51, 0x08, 0x00, 0x03, 0x1A, 0x00, 0x03, 0x00, 0x04, 0x0A]);
        let mut bat = [0u8; 64];
        bat[..13].copy_from_slice(&[0x51, 0x0B, 0, 0, 0, 0, 0, 0, 0x04, 0, 64, 0, 0x02]);
        assert_eq!(legacy_reply(&bat), Some(Legacy::Battery(64, true)));
        assert_eq!(legacy_reply(&[0x51, 0x05, 0, 0, 0, 0, 0x00]), Some(Legacy::Off));
        assert_eq!(legacy_reply(&[0x51, 0x05, 0, 0, 0, 0, 0x01]), Some(Legacy::Skip));
        assert_eq!(legacy_reply(&[0x51, 0x03, 0, 0]), Some(Legacy::Skip));
        assert_eq!(legacy_reply(&[0x52, 0x0B]), None);
    }
}

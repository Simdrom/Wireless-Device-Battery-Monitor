//! Device dashboard: one window per device with its level, charging state, connection, the
//! battery history of the last 7 days (charts with charging marked), charge / discharge sessions
//! and statistics. Opened from the device's right-click menu.
//!
//! It runs as its own short-lived process (`batteryhub-taskbar.exe --dashboard <tab> <device>`),
//! so the taskbar program keeps its small footprint and gets back nothing to free when it closes.
//! It reads the history file; the taskbar program sends it fresh readings (WM_COPYDATA) and tells
//! it when the history file was written. Painting happens only when something changed, the
//! timer runs only while an animation runs, and the hover crosshair redraws just the chart.
use std::cell::RefCell;
use std::ffi::c_void;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_ESCAPE, VK_TAB};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, w};

use crate::device::{DeviceKind, DeviceStatus};
use crate::gui::{self, Buffer, Face, Fonts, Hits, Theme, Tween, rect};
use crate::history::{self, Point};
use crate::stats::{self, Stats, fmt_hours, fmt_rate};

pub const CLASS: PCWSTR = w!("BatteryHub.Dashboard");
/// Posted by the taskbar program: the history file was written (read it again).
pub const WM_DASH_RELOAD: u32 = WM_APP + 30;
/// Posted by the taskbar program: show tab `wparam` and come to the front.
pub const WM_DASH_TAB: u32 = WM_APP + 31;
const COPY_STATUSES: usize = 1;
const TIMER_ANIM: usize = 1;
const KEY_PROP: PCWSTR = w!("BatteryHub.DeviceKey");
const SETTINGS_KEY: &str = crate::install::TASKBAR.settings_key;
const EDITION: &str = crate::install::TASKBAR.short;

const TABS: [&str; 3] = ["Overview", "History", "Statistics"];
const RANGES: [(&str, u64); 3] = [("24 hours", 24 * 3600), ("3 days", 3 * 24 * 3600), ("7 days", 7 * 24 * 3600)];
const ID_TAB: u32 = 1; // + tab
const ID_RANGE: u32 = 10; // + range
const ID_RENAME: u32 = 20;
const ID_CHART: u32 = 30;

// ---- status passed between the processes -------------------------------------------------------

const SEP: char = '\u{1F}';

/// One device's status (and the name it reports itself, when the user renamed it) as one line.
pub fn encode_status(s: &DeviceStatus, reported: &str) -> String {
    let clean = |t: &str| t.replace([SEP, '\n'], " ");
    let kind = s.kind as u8;
    let hw = s.hw.map(|(v, p)| format!("{v:04X}:{p:04X}")).unwrap_or_default();
    [
        clean(&s.key),
        clean(&s.name),
        clean(reported),
        clean(&s.via),
        kind.to_string(),
        s.percent.to_string(),
        clean(&s.detail),
        clean(&s.level_text),
        (s.charging as u8).to_string(),
        (s.full as u8).to_string(),
        (s.asleep as u8).to_string(),
        (s.connected as u8).to_string(),
        s.millivolts.to_string(),
        hw,
        s.dock_percent.map(|p| p.to_string()).unwrap_or_default(),
    ]
    .join(&SEP.to_string())
}

/// Reads a line of `encode_status`: (status, reported name).
pub fn decode_status(line: &str) -> Option<(DeviceStatus, String)> {
    let f: Vec<&str> = line.split(SEP).collect();
    if f.len() < 14 || f[0].is_empty() {
        return None;
    }
    let kind = match f[4].parse::<u8>().ok()? {
        1 => DeviceKind::Mouse,
        2 => DeviceKind::Headset,
        3 => DeviceKind::Keyboard,
        4 => DeviceKind::Gamepad,
        5 => DeviceKind::Phone,
        6 => DeviceKind::Microphone,
        _ => DeviceKind::Other,
    };
    let hw = f[13].split_once(':').and_then(|(v, p)| Some((u16::from_str_radix(v, 16).ok()?, u16::from_str_radix(p, 16).ok()?)));
    let s = DeviceStatus {
        key: f[0].into(),
        name: f[1].into(),
        via: f[3].into(),
        kind,
        percent: f[5].parse().unwrap_or(-1),
        detail: f[6].into(),
        level_text: f[7].into(),
        charging: f[8] == "1",
        full: f[9] == "1",
        asleep: f[10] == "1",
        connected: f[11] == "1",
        millivolts: f[12].parse().unwrap_or(0),
        hw,
        dock_percent: f.get(14).and_then(|p| p.parse().ok()),
        ..Default::default()
    };
    Some((s, f[2].to_string()))
}

// ---- used by the taskbar program ---------------------------------------------------------------

fn dashboards() -> Vec<HWND> {
    let mut out = Vec::new();
    unsafe {
        let mut after: Option<HWND> = None;
        while let Ok(h) = FindWindowExW(None, after, CLASS, None) {
            if h.0.is_null() {
                break;
            }
            after = Some(h);
            out.push(h);
        }
    }
    out
}

pub fn any_open() -> bool {
    unsafe { FindWindowW(CLASS, None).is_ok_and(|h| !h.0.is_null()) }
}

pub fn notify_history_changed() {
    for h in dashboards() {
        unsafe {
            let _ = PostMessageW(Some(h), WM_DASH_RELOAD, WPARAM(0), LPARAM(0));
        }
    }
}

/// Sends the current statuses (status, reported name) to every open dashboard.
pub fn send_statuses(from: HWND, list: &[(DeviceStatus, String)]) {
    let all = dashboards();
    if all.is_empty() {
        return;
    }
    let text: String = list.iter().map(|(s, r)| encode_status(s, r)).collect::<Vec<_>>().join("\n");
    let bytes = text.into_bytes();
    let cds = COPYDATASTRUCT { dwData: COPY_STATUSES, cbData: bytes.len() as u32, lpData: bytes.as_ptr() as *mut c_void };
    for h in all {
        unsafe {
            // A dashboard that hangs must not stall the taskbar: time out after 200 ms.
            let _ = SendMessageTimeoutW(h, WM_COPYDATA, WPARAM(from.0 as usize), LPARAM(&cds as *const _ as isize), SMTO_ABORTIFHUNG, 200, None);
        }
    }
}

/// Opens the dashboard of `s` on `tab`, or brings its open dashboard to the front on that tab.
pub fn open(s: &DeviceStatus, reported: &str, tab: u8) {
    let id = crate::util::fnv1a(&s.key) as usize | 1;
    for h in dashboards() {
        unsafe {
            if GetPropW(h, KEY_PROP).0 as usize == id {
                let mut pid = 0u32;
                GetWindowThreadProcessId(h, Some(&mut pid));
                let _ = windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(pid);
                let _ = PostMessageW(Some(h), WM_DASH_TAB, WPARAM(tab as usize), LPARAM(0));
                return;
            }
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).arg("--dashboard").arg(tab.to_string()).arg(encode_status(s, reported)).spawn();
    }
}

// ---- local time --------------------------------------------------------------------------------

/// Local wall-clock time of a unix time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Local {
    pub year: u16,
    pub month: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
    /// 0 = Sunday.
    pub weekday: u16,
}

pub fn local(t: u64) -> Local {
    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    let ft_val = (t + 11_644_473_600) * 10_000_000;
    let ft = FILETIME { dwLowDateTime: ft_val as u32, dwHighDateTime: (ft_val >> 32) as u32 };
    let (mut utc, mut loc) = (SYSTEMTIME::default(), SYSTEMTIME::default());
    unsafe {
        if FileTimeToSystemTime(&ft, &mut utc).is_err() || SystemTimeToTzSpecificLocalTime(None, &utc, &mut loc).is_err() {
            loc = utc;
        }
    }
    Local { year: loc.wYear, month: loc.wMonth, day: loc.wDay, hour: loc.wHour, minute: loc.wMinute, second: loc.wSecond, weekday: loc.wDayOfWeek }
}

/// Unix time of the local midnight that starts the day of `t`.
pub fn local_midnight(t: u64) -> u64 {
    let l = local(t);
    t - (l.hour as u64 * 3600 + l.minute as u64 * 60 + l.second as u64)
}

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// "14:02" today, "Tue 14:02" within a week, else "Nov 17 14:02".
pub fn fmt_when(t: u64, now: u64) -> String {
    let l = local(t);
    if local_midnight(t) == local_midnight(now) {
        format!("{:02}:{:02}", l.hour, l.minute)
    } else if now.saturating_sub(t) < 6 * 86_400 {
        format!("{} {:02}:{:02}", DAYS[l.weekday as usize % 7], l.hour, l.minute)
    } else {
        const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
        format!("{} {} {:02}:{:02}", MONTHS[(l.month as usize).clamp(1, 12) - 1], l.day, l.hour, l.minute)
    }
}

/// "5 min ago", "3 h ago", "2 d ago".
pub fn fmt_ago(t: u64, now: u64) -> String {
    let s = now.saturating_sub(t);
    if s < 90 {
        "just now".into()
    } else if s < 3600 {
        format!("{} min ago", s / 60)
    } else if s < 48 * 3600 {
        format!("{} h ago", s / 3600)
    } else {
        format!("{} d ago", s / 86_400)
    }
}

// ---- the window --------------------------------------------------------------------------------

struct Dash {
    hwnd: HWND,
    theme: Theme,
    s: f32,
    fonts: Fonts,
    /// Everything except the hover crosshair; repainted only when `dirty`.
    base: Option<Buffer>,
    dirty: bool,
    /// The chart area with the crosshair, while the mouse is over the chart.
    overlay: Option<Buffer>,
    status: DeviceStatus,
    reported: String,
    series: Vec<Point>,
    stats: Stats,
    tab: usize,
    range: usize,
    /// x of the tab underline, and the chart drawing itself in from the left.
    underline: Tween,
    underline_w: Tween,
    reveal: Tween,
    hits: Hits,
    hover: Option<u32>,
    tracking: bool,
    /// The chart's plot area (px) and its time range.
    chart: Option<(RECT, u64, u64)>,
    mouse: Option<POINT>,
}

thread_local! {
    static DASH: RefCell<Option<Dash>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut Dash) -> R) -> Option<R> {
    DASH.with(|d| d.try_borrow_mut().ok().and_then(|mut d| d.as_mut().map(f)))
}

impl Dash {
    fn px(&self, v: f32) -> i32 {
        (v * self.s).round() as i32
    }

    fn load_history(&mut self) {
        let h = std::fs::read(history::History::path(EDITION)).map(|b| history::History::decode(&b)).unwrap_or_default();
        // Only this device's points are kept.
        self.series = h.series(&self.status.key);
        self.recompute();
    }

    fn recompute(&mut self) {
        let current = (self.status.connected && !self.status.asleep).then_some((self.status.percent, self.status.charging && !self.status.is_full()));
        self.stats = stats::compute(&self.series, current);
        self.dirty = true;
    }

    fn invalidate(&mut self) {
        self.dirty = true;
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    fn animating(&self) -> bool {
        self.underline.running() || self.underline_w.running() || self.reveal.running()
    }

    fn set_tab(&mut self, tab: usize) {
        if tab == self.tab || tab >= TABS.len() {
            return;
        }
        self.tab = tab;
        self.reveal = Tween::new(0.0);
        self.reveal.go(1.0, 450.0);
        self.start_anim();
        self.invalidate();
    }

    fn start_anim(&self) {
        unsafe {
            SetTimer(Some(self.hwnd), TIMER_ANIM, 15, None);
        }
    }

    fn client(&self) -> (i32, i32) {
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut rc);
        }
        (rc.right - rc.left, rc.bottom - rc.top)
    }

    // ---- painting --------------------------------------------------------------------------

    fn render(&mut self) {
        let (w, h) = self.client();
        if self.base.as_ref().is_none_or(|b| b.w != w || b.h != h) {
            self.base = Buffer::new(w, h);
            self.overlay = None;
        }
        let Some(mut b) = self.base.take() else { return };
        self.hits.clear();
        let t = self.theme;
        b.clear(t.bg);
        let pad = self.px(28.0);
        let y = self.header(&mut b, pad, w);
        let y = self.tabs(&mut b, pad, y, w);
        match self.tab {
            0 => self.overview(&mut b, pad, y, w, h),
            1 => self.history_tab(&mut b, pad, y, w, h),
            _ => self.stats_tab(&mut b, pad, y, w, h),
        }
        self.base = Some(b);
        self.dirty = false;
    }

    fn header(&mut self, b: &mut Buffer, pad: i32, w: i32) -> i32 {
        let t = self.theme;
        let d = self.status.clone();
        let top = self.px(24.0);
        // Device glyph in a tinted circle.
        let r = self.px(26.0) as f32;
        let (cx, cy) = (pad as f32 + r, top as f32 + r);
        b.circle(cx, cy, r, gui::mix(t.card, t.accent, 0.18), 1.0);
        let gf = self.fonts.get(self.px(24.0), 400, Face::Icons);
        b.text(&d.kind.glyph().to_string(), gf, t.accent, rect(pad, top, 2 * r as i32, 2 * r as i32), DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        let x = pad + 2 * r as i32 + self.px(16.0);
        // Right side: the level and the state.
        let right = w - pad;
        let big = self.fonts.get(self.px(34.0), 600, Face::Display);
        let level = d.level();
        let lw = b.measure(&level, big).cx;
        let low = crate::settings::is_low(&d, crate::settings::threshold(SETTINGS_KEY, &d.key));
        let level_c = if !d.connected {
            t.faint
        } else if low {
            t.red
        } else {
            t.text
        };
        b.text(&level, big, level_c, rect(right - lw, top - self.px(4.0), lw, self.px(44.0)), DT_RIGHT | DT_SINGLELINE);
        let (state, state_c) = state_label(&d, &t);
        let small = self.fonts.get(self.px(13.0), 400, Face::Text);
        let sw = b.measure(state, small).cx;
        let chip_w = sw + self.px(20.0);
        let chip_y = top + self.px(42.0);
        b.rrect((right - chip_w) as f32, chip_y as f32, chip_w as f32, self.px(22.0) as f32, self.px(11.0) as f32, gui::mix(t.card, state_c, 0.18), 1.0);
        b.text(state, small, state_c, rect(right - chip_w, chip_y, chip_w, self.px(22.0)), DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        // Level bar under the number.
        let bar_w = self.px(150.0).max(lw);
        let bar_y = chip_y + self.px(32.0);
        b.rrect((right - bar_w) as f32, bar_y as f32, bar_w as f32, self.px(6.0) as f32, self.px(3.0) as f32, t.track, 1.0);
        if d.percent > 0 && d.connected {
            let fill = (bar_w as f32 * d.percent.min(100) as f32 / 100.0).max(self.px(6.0) as f32);
            let c = if low {
                t.red
            } else if d.charging {
                t.blue
            } else {
                t.green
            };
            b.rrect((right - bar_w) as f32, bar_y as f32, fill, self.px(6.0) as f32, self.px(3.0) as f32, c, 1.0);
        }
        // Name, then what and how it is connected.
        let name_room = right - bar_w.max(chip_w) - self.px(24.0) - x;
        let nf = self.fonts.get(self.px(26.0), 600, Face::Display);
        b.text(&d.name, nf, t.text, rect(x, top - self.px(2.0), name_room, self.px(36.0)), DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS);
        let mut sub = format!("{} · {}", d.kind.label(), if d.via.is_empty() { "Connected" } else { &d.via });
        if !self.reported.is_empty() && self.reported != d.name {
            sub = format!("{sub} · reports itself as \"{}\"", self.reported);
        }
        let sf = self.fonts.get(self.px(14.0), 400, Face::Text);
        b.text(&sub, sf, t.dim, rect(x, top + self.px(36.0), name_room, self.px(20.0)), DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS);
        // "Rename" as a small text button.
        let rf = self.fonts.get(self.px(13.0), 400, Face::Text);
        let label = "Rename...";
        let rw = b.measure(label, rf).cx + self.px(16.0);
        let rr = rect(x - self.px(8.0), top + self.px(60.0), rw, self.px(24.0));
        if self.hover == Some(ID_RENAME) {
            b.rrect(rr.left as f32, rr.top as f32, rw as f32, self.px(24.0) as f32, self.px(4.0) as f32, t.button_hover, 1.0);
        }
        b.text(label, rf, t.accent, rr, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        self.hits.add(rr, ID_RENAME);
        top + self.px(96.0)
    }

    fn tabs(&mut self, b: &mut Buffer, pad: i32, y: i32, w: i32) -> i32 {
        let t = self.theme;
        let f = self.fonts.get(self.px(14.0), 400, Face::Text);
        let fsel = self.fonts.get(self.px(14.0), 600, Face::Text);
        let mut x = pad;
        let h = self.px(36.0);
        for (i, name) in TABS.iter().enumerate() {
            let tw = b.measure(name, fsel).cx + self.px(24.0);
            let r = rect(x, y, tw, h);
            if self.hover == Some(ID_TAB + i as u32) && i != self.tab {
                b.rrect(x as f32, (y + self.px(2.0)) as f32, tw as f32, (h - self.px(4.0)) as f32, self.px(4.0) as f32, t.button_hover, 1.0);
            }
            let sel = i == self.tab;
            b.text(name, if sel { fsel } else { f }, if sel { t.text } else { t.dim }, r, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
            if sel {
                // The underline glides to the selected tab.
                let target = (x + self.px(12.0)) as f32;
                let tw2 = (tw - self.px(24.0)) as f32;
                if (self.underline.to - target).abs() > 0.5 {
                    if self.underline.to == 0.0 {
                        self.underline = Tween::new(target);
                        self.underline_w = Tween::new(tw2);
                    } else {
                        self.underline.go(target, 220.0);
                        self.underline_w.go(tw2, 220.0);
                    }
                }
            }
            self.hits.add(r, ID_TAB + i as u32);
            x += tw + self.px(4.0);
        }
        let line_y = y + h;
        b.fill(pad, line_y, w - 2 * pad, self.px(1.0).max(1), t.stroke);
        b.rrect(self.underline.value(), (line_y - self.px(3.0)) as f32, self.underline_w.value(), self.px(3.0) as f32, self.px(1.5) as f32, t.accent, 1.0);
        line_y + self.px(20.0)
    }

    /// A statistic card: icon, title, value (or "not enough data") and an explanation.
    #[allow(clippy::too_many_arguments)]
    fn stat_card(&mut self, b: &mut Buffer, r: RECT, icon: char, title: &str, value: Option<String>, note: &str) {
        let t = self.theme;
        b.card(r.left as f32, r.top as f32, (r.right - r.left) as f32, (r.bottom - r.top) as f32, self.px(8.0) as f32, t.card, t.stroke);
        let p = self.px(16.0);
        let gf = self.fonts.get(self.px(16.0), 400, Face::Icons);
        b.text(&icon.to_string(), gf, t.accent, rect(r.left + p, r.top + p, self.px(20.0), self.px(20.0)), DT_LEFT | DT_SINGLELINE);
        let tf = self.fonts.get(self.px(13.0), 400, Face::Text);
        b.text(
            title,
            tf,
            t.dim,
            rect(r.left + p + self.px(26.0), r.top + p, r.right - r.left - 2 * p - self.px(26.0), self.px(20.0)),
            DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
        let (text, c, f) = match &value {
            Some(v) => (v.as_str(), t.text, self.fonts.get(self.px(24.0), 600, Face::Display)),
            None => ("Not enough data yet", t.faint, self.fonts.get(self.px(15.0), 400, Face::Text)),
        };
        b.text(
            text,
            f,
            c,
            rect(r.left + p, r.top + p + self.px(28.0), r.right - r.left - 2 * p, self.px(32.0)),
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
        );
        let nf = self.fonts.get(self.px(12.0), 400, Face::Text);
        b.text(
            note,
            nf,
            t.faint,
            rect(r.left + p, r.top + p + self.px(62.0), r.right - r.left - 2 * p, r.bottom - r.top - p - self.px(62.0) - self.px(8.0)),
            DT_LEFT | DT_WORDBREAK | DT_END_ELLIPSIS,
        );
    }

    /// Time left: on battery from the average rate; while charging, the time to full.
    fn time_left(&self) -> (Option<String>, String) {
        let st = &self.stats;
        let d = &self.status;
        if !d.connected {
            return (None, "The device is not connected.".into());
        }
        if d.is_full() {
            return (Some("Full".into()), "On power and fully charged.".into());
        }
        if d.charging {
            return match st.to_full_h {
                Some(h) => (Some(fmt_hours(h)), "Until full, at the average charge rate.".into()),
                None => (None, "Charging. The time to full needs a measured charge first.".into()),
            };
        }
        match st.remaining_h {
            Some(h) => (Some(fmt_hours(h)), format!("On battery, at {} (average).", fmt_rate(st.discharge_rate.unwrap_or(0.0)))),
            None => (None, "Needs about an hour of measured use on battery.".into()),
        }
    }

    fn counted_discharges(&self) -> (usize, u64) {
        let v: Vec<_> =
            self.stats.discharges.iter().filter(|s| s.from >= s.to.saturating_add(stats::MIN_DROP) && s.secs() >= stats::MIN_DISCHARGE_SECS).collect();
        (v.len(), v.iter().map(|s| s.secs()).sum())
    }

    fn counted_charges(&self) -> usize {
        self.stats.charges.iter().filter(|s| s.to >= s.from.saturating_add(stats::MIN_RISE) && s.secs() >= stats::MIN_CHARGE_SECS).count()
    }

    fn overview(&mut self, b: &mut Buffer, pad: i32, y: i32, w: i32, h: i32) {
        let now = history::now_secs();
        let gap = self.px(12.0);
        let card_h = self.px(118.0);
        let cw = (w - 2 * pad - 3 * gap) / 4;
        let (tl, tl_note) = self.time_left();
        let (n, secs) = self.counted_discharges();
        let st = self.stats.clone();
        let cards: [(char, &str, Option<String>, String); 4] = [
            ('\u{E916}', "Time left", tl, tl_note),
            (
                '\u{E83F}',
                "Battery life",
                st.battery_life_h.map(fmt_hours),
                if n > 0 {
                    format!("Full to empty, from {n} session{} ({}).", if n == 1 { "" } else { "s" }, fmt_hours(secs as f64 / 3600.0))
                } else {
                    "Full to empty at the average rate.".into()
                },
            ),
            ('\u{E9D2}', "Discharge rate", st.discharge_rate.map(fmt_rate), "Average drop while on battery.".into()),
            (
                '\u{E945}',
                "Last charged",
                if self.status.charging { Some("Now".into()) } else { st.last_charged.map(|t| fmt_ago(t, now)) },
                st.last_charged.map(|t| format!("Charge ended {}.", fmt_when(t, now))).unwrap_or_else(|| "No charge recorded in the last 7 days.".into()),
            ),
        ];
        for (i, (icon, title, value, note)) in cards.into_iter().enumerate() {
            let r = rect(pad + i as i32 * (cw + gap), y, cw, card_h);
            self.stat_card(b, r, icon, title, value, &note);
        }
        let y = y + card_h + gap;
        // Info card at the bottom, the chart fills the rest.
        let info_h = self.px(118.0);
        let chart_h = (h - y - info_h - gap - pad).max(self.px(160.0));
        self.chart_card(b, rect(pad, y, w - 2 * pad, chart_h), "Last 24 hours", 24 * 3600, false);
        self.info_card(b, rect(pad, y + chart_h + gap, w - 2 * pad, info_h));
    }

    fn info_card(&mut self, b: &mut Buffer, r: RECT) {
        let t = self.theme;
        let d = self.status.clone();
        b.card(r.left as f32, r.top as f32, (r.right - r.left) as f32, (r.bottom - r.top) as f32, self.px(8.0) as f32, t.card, t.stroke);
        let p = self.px(16.0);
        let hf = self.fonts.get(self.px(14.0), 600, Face::Text);
        b.text("Device information", hf, t.text, rect(r.left + p, r.top + p - self.px(2.0), self.px(300.0), self.px(20.0)), DT_LEFT | DT_SINGLELINE);
        let now = history::now_secs();
        let mut rows: Vec<(&str, String)> = vec![
            ("Type", d.kind.label().into()),
            ("Connection", if d.via.is_empty() { "-".into() } else { d.via.clone() }),
            ("Hardware ID", d.hw.map(|(v, p)| format!("{v:04X}:{p:04X}")).unwrap_or_else(|| "-".into())),
            ("Readings stored", format!("{} (7 days)", self.stats.readings)),
            ("History since", self.stats.first_reading.map(|t| fmt_when(t, now)).unwrap_or_else(|| "-".into())),
            ("Device key", d.key.clone()),
        ];
        if !d.detail.is_empty() {
            rows.insert(2, ("Details", d.detail.clone()));
        }
        if let Some(dock) = d.dock_text() {
            rows.insert(2, ("Dock battery", dock));
        }
        if d.millivolts > 0 {
            rows.insert(2, ("Battery voltage", format!("{} mV", d.millivolts)));
        }
        let lf = self.fonts.get(self.px(12.0), 400, Face::Text);
        let vf = self.fonts.get(self.px(13.0), 400, Face::Text);
        let cols = 3;
        let col_w = (r.right - r.left - 2 * p) / cols;
        for (i, (label, value)) in rows.iter().take(9).enumerate() {
            let (cx, cy) = (r.left + p + (i as i32 % cols) * col_w, r.top + p + self.px(26.0) + (i as i32 / cols) * self.px(26.0));
            let lw = self.px(110.0);
            b.text(label, lf, t.faint, rect(cx, cy, lw, self.px(20.0)), DT_LEFT | DT_SINGLELINE | DT_VCENTER);
            b.text(value, vf, t.text, rect(cx + lw, cy, col_w - lw - self.px(8.0), self.px(20.0)), DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS);
        }
    }

    fn history_tab(&mut self, b: &mut Buffer, pad: i32, y: i32, w: i32, h: i32) {
        let t = self.theme;
        let gap = self.px(12.0);
        // Range selector.
        let f = self.fonts.get(self.px(13.0), 400, Face::Text);
        let mut x = pad;
        let rh = self.px(28.0);
        let widths: Vec<i32> = RANGES.iter().map(|(n, _)| b.measure(n, f).cx + self.px(24.0)).collect();
        let total: i32 = widths.iter().sum();
        b.card(x as f32, y as f32, total as f32, rh as f32, self.px(6.0) as f32, t.card, t.stroke);
        for (i, (name, _)) in RANGES.iter().enumerate() {
            let r = rect(x, y, widths[i], rh);
            if i == self.range {
                b.rrect(
                    (x + self.px(3.0)) as f32,
                    (y + self.px(3.0)) as f32,
                    (widths[i] - self.px(6.0)) as f32,
                    (rh - self.px(6.0)) as f32,
                    self.px(4.0) as f32,
                    gui::mix(t.card, t.accent, 0.25),
                    1.0,
                );
            } else if self.hover == Some(ID_RANGE + i as u32) {
                b.rrect(
                    (x + self.px(3.0)) as f32,
                    (y + self.px(3.0)) as f32,
                    (widths[i] - self.px(6.0)) as f32,
                    (rh - self.px(6.0)) as f32,
                    self.px(4.0) as f32,
                    t.card_hover,
                    1.0,
                );
            }
            b.text(name, f, if i == self.range { t.text } else { t.dim }, r, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
            self.hits.add(r, ID_RANGE + i as u32);
            x += widths[i];
        }
        let y = y + rh + gap;
        let lists_h = self.px(210.0);
        let chart_h = (h - y - lists_h - gap - pad).max(self.px(200.0));
        let (label, span) = RANGES[self.range];
        self.chart_card(b, rect(pad, y, w - 2 * pad, chart_h), &format!("Battery level, last {label}"), span, true);
        let y = y + chart_h + gap;
        let half = (w - 2 * pad - gap) / 2;
        self.sessions_card(b, rect(pad, y, half, lists_h), true);
        self.sessions_card(b, rect(pad + half + gap, y, half, lists_h), false);
    }

    fn sessions_card(&mut self, b: &mut Buffer, r: RECT, charging: bool) {
        let t = self.theme;
        let now = history::now_secs();
        b.card(r.left as f32, r.top as f32, (r.right - r.left) as f32, (r.bottom - r.top) as f32, self.px(8.0) as f32, t.card, t.stroke);
        let p = self.px(16.0);
        let hf = self.fonts.get(self.px(14.0), 600, Face::Text);
        let gf = self.fonts.get(self.px(14.0), 400, Face::Icons);
        let (icon, title, c) = if charging { ('\u{E945}', "Charging history", t.blue) } else { ('\u{E83F}', "Discharge history", t.green) };
        b.text(&icon.to_string(), gf, c, rect(r.left + p, r.top + p, self.px(20.0), self.px(20.0)), DT_LEFT | DT_SINGLELINE);
        b.text(title, hf, t.text, rect(r.left + p + self.px(24.0), r.top + p - self.px(1.0), self.px(300.0), self.px(20.0)), DT_LEFT | DT_SINGLELINE);
        let list: Vec<stats::Session> = if charging {
            self.stats.charges.iter().rev().filter(|s| s.to > s.from || s.completed).copied().collect()
        } else {
            self.stats.discharges.iter().rev().filter(|s| s.from > s.to).copied().collect()
        };
        let lf = self.fonts.get(self.px(13.0), 400, Face::Text);
        let row_h = self.px(26.0);
        let mut yy = r.top + p + self.px(30.0);
        if list.is_empty() {
            let msg = if charging { "No charging recorded in the last 7 days." } else { "No time on battery recorded yet." };
            b.text(msg, lf, t.faint, rect(r.left + p, yy, r.right - r.left - 2 * p, row_h), DT_LEFT | DT_SINGLELINE | DT_VCENTER);
            return;
        }
        let inner = r.right - r.left - 2 * p;
        for s in list.iter() {
            if yy + row_h > r.bottom - p / 2 {
                break;
            }
            let when = if charging { fmt_when(s.start, now) } else { format!("{} - {}", fmt_when(s.start, now), fmt_when(s.end, now)) };
            let levels = format!("{}% → {}%", s.from, s.to);
            let tail = if charging {
                format!("{}{}", fmt_hours(s.secs() as f64 / 3600.0), if s.completed { " · full" } else { "" })
            } else {
                format!("{} · {}", fmt_hours(s.secs() as f64 / 3600.0), fmt_rate(-s.rate()))
            };
            let c1 = inner * 46 / 100;
            let c2 = inner * 22 / 100;
            b.text(&when, lf, t.dim, rect(r.left + p, yy, c1, row_h), DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS);
            b.text(&levels, lf, t.text, rect(r.left + p + c1, yy, c2, row_h), DT_LEFT | DT_SINGLELINE | DT_VCENTER);
            b.text(&tail, lf, t.faint, rect(r.left + p + c1 + c2, yy, inner - c1 - c2, row_h), DT_RIGHT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS);
            yy += row_h;
            if yy + row_h <= r.bottom - p / 2 {
                b.fill(r.left + p, yy - 1, inner, 1, t.stroke);
            }
        }
    }

    fn stats_tab(&mut self, b: &mut Buffer, pad: i32, y: i32, w: i32, h: i32) {
        let now = history::now_secs();
        let gap = self.px(12.0);
        let cw = (w - 2 * pad - 2 * gap) / 3;
        let card_h = self.px(132.0);
        let st = self.stats.clone();
        let (n, secs) = self.counted_discharges();
        let nc = self.counted_charges();
        let (tl, tl_note) = self.time_left();
        let plural = |n: usize, w: &str| format!("{n} {w}{}", if n == 1 { "" } else { "s" });
        let cards: [(char, &str, Option<String>, String); 6] = [
            (
                '\u{E83F}',
                "Average battery life",
                st.battery_life_h.map(fmt_hours),
                match st.battery_life_h {
                    Some(_) => format!("Full to empty at the average rate, from {} on battery ({}).", plural(n, "session"), fmt_hours(secs as f64 / 3600.0)),
                    None => "Needs at least an hour of measured use on battery, with a drop of 5% or more.".into(),
                },
            ),
            ('\u{E916}', "Estimated time left", tl, tl_note),
            (
                '\u{E9D2}',
                "Average discharge rate",
                st.discharge_rate.map(fmt_rate),
                "Percent lost per hour of use on battery. Gaps in monitoring are left out.".into(),
            ),
            (
                '\u{E945}',
                "Average charge rate",
                st.charge_rate.map(fmt_rate),
                match nc {
                    0 => "Needs a measured charge of 5% or more.".into(),
                    n => format!("Percent gained per hour, from {}.", plural(n, "charging session")),
                },
            ),
            ('\u{E81C}', "Full charge (0 → 100%)", st.full_charge_h.map(fmt_hours), "Estimated from the average charge rate.".into()),
            (
                '\u{E823}',
                "Average charging session",
                st.avg_charge_session_h.map(fmt_hours),
                st.last_charged.map(|t| format!("Last charge ended {}.", fmt_when(t, now))).unwrap_or_else(|| "No charge recorded in the last 7 days.".into()),
            ),
        ];
        for (i, (icon, title, value, note)) in cards.into_iter().enumerate() {
            let r = rect(pad + (i as i32 % 3) * (cw + gap), y + (i as i32 / 3) * (card_h + gap), cw, card_h);
            self.stat_card(b, r, icon, title, value, &note);
        }
        // Monitoring summary.
        let t = self.theme;
        let y = y + 2 * (card_h + gap);
        let r = rect(pad, y, w - 2 * pad, (h - y - pad).clamp(self.px(90.0), self.px(150.0)));
        b.card(r.left as f32, r.top as f32, (r.right - r.left) as f32, (r.bottom - r.top) as f32, self.px(8.0) as f32, t.card, t.stroke);
        let p = self.px(16.0);
        let hf = self.fonts.get(self.px(14.0), 600, Face::Text);
        b.text(
            "How these figures are made",
            hf,
            t.text,
            rect(r.left + p, r.top + p - self.px(2.0), r.right - r.left - 2 * p, self.px(20.0)),
            DT_LEFT | DT_SINGLELINE,
        );
        let nf = self.fonts.get(self.px(13.0), 400, Face::Text);
        let text = format!(
            "{} readings over {} of monitoring{}. Only measured stretches count: time when BatteryHub was not running, the PC was off or the device was disconnected is left out, and nothing is filled in between readings. Sessions shorter than 30 minutes on battery or 5 minutes on power, or with a change under 5%, are not used for averages.",
            st.readings,
            fmt_hours(st.monitored_secs as f64 / 3600.0),
            st.first_reading.map(|t| format!(" since {}", fmt_when(t, now))).unwrap_or_default()
        );
        b.text(
            &text,
            nf,
            t.dim,
            rect(r.left + p, r.top + p + self.px(26.0), r.right - r.left - 2 * p, r.bottom - r.top - p - self.px(30.0)),
            DT_LEFT | DT_WORDBREAK,
        );
    }

    /// Card with a step chart of the level over `span` seconds up to now: green on battery, blue
    /// while charging (with a band behind it), nothing where there was no reading.
    fn chart_card(&mut self, b: &mut Buffer, r: RECT, title: &str, span: u64, markers: bool) {
        let t = self.theme;
        b.card(r.left as f32, r.top as f32, (r.right - r.left) as f32, (r.bottom - r.top) as f32, self.px(8.0) as f32, t.card, t.stroke);
        let p = self.px(16.0);
        let hf = self.fonts.get(self.px(14.0), 600, Face::Text);
        b.text(title, hf, t.text, rect(r.left + p, r.top + p - self.px(2.0), r.right - r.left - 2 * p, self.px(20.0)), DT_LEFT | DT_SINGLELINE);
        // Legend on the right of the title.
        let lf = self.fonts.get(self.px(12.0), 400, Face::Text);
        let mut lx = r.right - p;
        for (label, c) in [("Charging", t.blue), ("On battery", t.green)] {
            let lw = b.measure(label, lf).cx;
            lx -= lw;
            b.text(label, lf, t.dim, rect(lx, r.top + p - self.px(1.0), lw + 2, self.px(18.0)), DT_LEFT | DT_SINGLELINE);
            lx -= self.px(14.0);
            b.rrect(lx as f32, (r.top + p + self.px(4.0)) as f32, self.px(10.0) as f32, self.px(10.0) as f32, self.px(3.0) as f32, c, 1.0);
            lx -= self.px(16.0);
        }
        let axis_w = self.px(40.0);
        let plot = RECT { left: r.left + p + axis_w, top: r.top + p + self.px(34.0), right: r.right - p, bottom: r.bottom - p - self.px(22.0) };
        if plot.right - plot.left < 50 || plot.bottom - plot.top < 40 {
            return;
        }
        let now = history::now_secs();
        let from = now - span;
        self.chart = Some((plot, from, now));
        self.hits.add(plot, ID_CHART);
        let (x0, x1, y0, y1) = (plot.left as f32, plot.right as f32, plot.top as f32, plot.bottom as f32);
        let to_x = |tt: u64| x0 + (x1 - x0) * (tt.saturating_sub(from) as f32 / span as f32).clamp(0.0, 1.0);
        let to_y = |l: u8| y1 - (y1 - y0) * l as f32 / 100.0;
        // Grid: 0, 50, 100 % with labels.
        let af = self.fonts.get(self.px(11.0), 400, Face::Text);
        for l in [0u8, 25, 50, 75, 100] {
            let y = to_y(l).round() as i32;
            let c = if l % 50 == 0 { t.stroke } else { gui::mix(t.card, t.stroke, 0.5) };
            b.fill(plot.left, y, plot.right - plot.left, 1, c);
            if l % 50 == 0 {
                b.text(
                    &format!("{l}%"),
                    af,
                    t.faint,
                    rect(r.left + p, y - self.px(8.0), axis_w - self.px(8.0), self.px(16.0)),
                    DT_RIGHT | DT_SINGLELINE | DT_VCENTER,
                );
            }
        }
        // Time axis: every 6 h for a day, else local midnights.
        let mut ticks: Vec<(u64, String)> = Vec::new();
        if span <= 24 * 3600 {
            let mut tt = local_midnight(from);
            while tt <= now {
                if tt >= from {
                    let l = local(tt);
                    ticks.push((tt, format!("{:02}:00", l.hour)));
                }
                tt += 6 * 3600;
            }
        } else {
            let mut tt = local_midnight(from) + 86_400;
            while tt <= now {
                // Days are not always 24 h long (daylight saving): find the real midnight.
                let m = local_midnight(tt + 3 * 3600);
                ticks.push((m, DAYS[local(m).weekday as usize % 7].to_string()));
                tt = m + 86_400;
            }
        }
        for (tt, label) in &ticks {
            let x = to_x(*tt).round() as i32;
            let mut yy = plot.top;
            while yy < plot.bottom {
                b.fill(x, yy, 1, self.px(2.0).max(1), t.stroke);
                yy += self.px(5.0).max(3);
            }
            let lw = b.measure(label, af).cx;
            b.text(label, af, t.faint, rect(x - lw / 2, plot.bottom + self.px(5.0), lw + 2, self.px(16.0)), DT_LEFT | DT_SINGLELINE);
        }
        // Data, drawn in from the left while `reveal` runs.
        let limit = x0 + (x1 - x0) * gui::ease_out(self.reveal.value());
        let segs = history::segments(&self.series, from, now);
        let lw = (2.0 * self.s).max(1.5);
        if segs.is_empty() {
            let mf = self.fonts.get(self.px(13.0), 400, Face::Text);
            b.text("No readings in this period yet.", mf, t.faint, plot, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        }
        for seg in &segs {
            // The last reading holds until the next one; the newest one is drawn up to its time.
            for (i, a) in seg.iter().enumerate() {
                let end = seg.get(i + 1).map(|n| n.t).unwrap_or(a.t);
                let (xa, xb) = (to_x(a.t), to_x(end).min(limit));
                if xa > limit {
                    break;
                }
                let c = if a.charging() { t.blue } else { t.green };
                if a.charging() && xb > xa {
                    b.rrect(xa, y0, xb - xa, y1 - y0, 0.0, c, 0.10);
                }
                let ya = to_y(a.level);
                for col in xa.round() as i32..xb.round() as i32 {
                    b.column_fade(col, ya, y1, c, 0.22);
                }
                b.line(xa, ya, xb.max(xa), ya, lw, c, 1.0);
                if let Some(n) = seg.get(i + 1)
                    && to_x(n.t) <= limit
                {
                    let nc = if n.charging() { t.blue } else { t.green };
                    b.line(to_x(n.t), ya, to_x(n.t), to_y(n.level), lw, nc, 1.0);
                }
                // Charge start / end markers.
                if markers {
                    let prev_chg = i > 0 && seg[i - 1].charging();
                    if a.charging() != prev_chg && i > 0 {
                        b.circle(xa, ya, 4.0 * self.s, t.card, 1.0);
                        b.circle(xa, ya, 3.0 * self.s, t.blue, 1.0);
                    }
                }
            }
            if let Some(last) = seg.last()
                && to_x(last.t) <= limit
            {
                let c = if last.charging() { t.blue } else { t.green };
                b.circle(to_x(last.t), to_y(last.level), 3.5 * self.s, c, 1.0);
            }
        }
    }

    // ---- hover crosshair -------------------------------------------------------------------

    /// Level shown at time `tt`, from the readings (None in a gap or after the last reading).
    fn level_at(&self, tt: u64) -> Option<Point> {
        let segs = history::segments(&self.series, tt.saturating_sub(8 * 86_400), history::now_secs());
        for seg in segs {
            let (Some(first), Some(last)) = (seg.first(), seg.last()) else { continue };
            if tt < first.t || tt > last.t + 90 {
                continue;
            }
            return seg.iter().rev().find(|p| p.t <= tt).copied();
        }
        None
    }

    /// Draws the chart area with the crosshair into the overlay and shows it.
    fn paint_overlay(&mut self, hdc: HDC) {
        let (Some((plot, from, to)), Some(m)) = (self.chart, self.mouse) else { return };
        let t = self.theme;
        let pad = self.px(150.0);
        // The area covers the plot plus room for the label box.
        let area = RECT { left: (plot.left - pad).max(0), top: plot.top - self.px(4.0), right: plot.right + self.px(4.0), bottom: plot.bottom + self.px(4.0) };
        let (aw, ah) = (area.right - area.left, area.bottom - area.top);
        if self.overlay.as_ref().is_none_or(|o| o.w != aw || o.h != ah) {
            self.overlay = Buffer::new(aw, ah);
        }
        let x = m.x.clamp(plot.left, plot.right);
        let tt = from + ((x - plot.left) as f64 / (plot.right - plot.left) as f64 * (to - from) as f64) as u64;
        let (ox, oy) = (x - area.left, plot.top - area.top);
        let ph = plot.bottom - plot.top;
        let (text, dot) = match self.level_at(tt) {
            Some(p) => {
                let y = plot.bottom - (ph as f32 * p.level as f32 / 100.0) as i32;
                (
                    format!("{}  ·  {}%  ·  {}", fmt_when(tt, history::now_secs()), p.level, if p.charging() { "charging" } else { "on battery" }),
                    Some((y, p.charging())),
                )
            }
            None => (format!("{}  ·  no reading", fmt_when(tt, history::now_secs())), None),
        };
        let f = self.fonts.get((12.0 * self.s).round() as i32, 400, Face::Text);
        let (Some(o), Some(base)) = (self.overlay.as_mut(), self.base.as_ref()) else { return };
        unsafe {
            let _ = BitBlt(o.dc, 0, 0, aw, ah, Some(base.dc), area.left, area.top, SRCCOPY);
        }
        o.fill(ox, oy, 1, ph, t.dim);
        if let Some((y, chg)) = dot {
            o.circle(ox as f32 + 0.5, (y - area.top) as f32, 5.0 * self.s, t.card, 1.0);
            o.circle(ox as f32 + 0.5, (y - area.top) as f32, 3.5 * self.s, if chg { t.blue } else { t.green }, 1.0);
        }
        let sz = o.measure(&text, f);
        let (bw, bh) = (sz.cx + (20.0 * self.s) as i32, sz.cy + (10.0 * self.s) as i32);
        let mut bx = ox + (10.0 * self.s) as i32;
        if bx + bw > aw {
            bx = ox - bw - (10.0 * self.s) as i32;
        }
        let by = oy + (6.0 * self.s) as i32;
        o.card(bx as f32, by as f32, bw as f32, bh as f32, 4.0 * self.s, t.card_hover, t.stroke);
        o.text(&text, f, t.text, rect(bx, by, bw, bh), DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        unsafe {
            let _ = BitBlt(hdc, area.left, area.top, aw, ah, Some(o.dc), 0, 0, SRCCOPY);
        }
    }
}

fn state_label(d: &DeviceStatus, t: &Theme) -> (&'static str, COLORREF) {
    if !d.connected {
        ("Disconnected", t.faint)
    } else if d.asleep {
        ("Asleep", t.faint)
    } else if d.is_full() {
        ("Fully charged", t.green)
    } else if d.charging {
        ("Charging", t.blue)
    } else {
        ("On battery", t.dim)
    }
}

fn hover_id(d: &Dash) -> Option<u32> {
    d.hover
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                with(|d| {
                    if d.dirty || d.animating() || d.base.is_none() {
                        d.render();
                    }
                    if let Some(b) = &d.base {
                        b.blit(hdc);
                    }
                    if d.mouse.is_some() && hover_id(d) == Some(ID_CHART) {
                        d.paint_overlay(hdc);
                    }
                });
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_TIMER if wp.0 == TIMER_ANIM => {
                let still = with(|d| {
                    d.dirty = true;
                    d.animating()
                })
                .unwrap_or(false);
                let _ = InvalidateRect(Some(hwnd), None, false);
                if !still {
                    let _ = KillTimer(Some(hwnd), TIMER_ANIM);
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let (x, y) = ((lp.0 & 0xFFFF) as i16 as i32, ((lp.0 >> 16) & 0xFFFF) as i16 as i32);
                with(|d| {
                    if !d.tracking {
                        let mut tme = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                        d.tracking = TrackMouseEvent(&mut tme).is_ok();
                    }
                    let h = d.hits.at(x, y);
                    let was = d.hover;
                    d.hover = h;
                    d.mouse = Some(POINT { x, y });
                    if h != was && (was.is_some_and(|i| i != ID_CHART) || h.is_some_and(|i| i != ID_CHART)) {
                        // A button's hover tint changed: repaint everything (rare).
                        d.invalidate();
                    } else if h == Some(ID_CHART) || was == Some(ID_CHART) {
                        // Only the crosshair moved: base stays, the chart area is drawn again.
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    let cursor = if h.is_some_and(|i| i != ID_CHART) { IDC_HAND } else { IDC_ARROW };
                    SetCursor(LoadCursorW(None, cursor).ok());
                });
                LRESULT(0)
            }
            0x02A3 /* WM_MOUSELEAVE */ => {
                with(|d| {
                    d.tracking = false;
                    d.mouse = None;
                    if d.hover.take().is_some() {
                        d.invalidate();
                    }
                });
                LRESULT(0)
            }
            WM_SETCURSOR if (lp.0 & 0xFFFF) as u32 == HTCLIENT => LRESULT(1),
            WM_LBUTTONUP => {
                let (x, y) = ((lp.0 & 0xFFFF) as i16 as i32, ((lp.0 >> 16) & 0xFFFF) as i16 as i32);
                let hit = with(|d| d.hits.at(x, y)).flatten();
                match hit {
                    Some(id) if (ID_TAB..ID_TAB + 3).contains(&id) => {
                        with(|d| d.set_tab((id - ID_TAB) as usize));
                    }
                    Some(id) if (ID_RANGE..ID_RANGE + 3).contains(&id) => {
                        with(|d| {
                            if d.range != (id - ID_RANGE) as usize {
                                d.range = (id - ID_RANGE) as usize;
                                d.reveal = Tween::new(0.0);
                                d.reveal.go(1.0, 450.0);
                                d.start_anim();
                                d.invalidate();
                            }
                        });
                    }
                    Some(ID_RENAME) => {
                        if let Some((s, reported)) = with(|d| (d.status.clone(), d.reported.clone())) {
                            crate::rename::open(&s, &reported);
                        }
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_KEYDOWN => {
                if wp.0 == VK_ESCAPE.0 as usize {
                    let _ = DestroyWindow(hwnd);
                } else if wp.0 == VK_TAB.0 as usize {
                    with(|d| d.set_tab((d.tab + 1) % TABS.len()));
                }
                LRESULT(0)
            }
            WM_COPYDATA => {
                let cds = &*(lp.0 as *const COPYDATASTRUCT);
                if cds.dwData == COPY_STATUSES && !cds.lpData.is_null() {
                    let bytes = std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize);
                    let text = String::from_utf8_lossy(bytes).to_string();
                    with(|d| {
                        let fresh = text.lines().filter_map(decode_status).find(|(s, _)| s.key == d.status.key);
                        match fresh {
                            Some((s, reported)) => {
                                d.status = s;
                                d.reported = reported;
                            }
                            None => d.status.connected = false,
                        }
                        set_title(hwnd, &d.status.name);
                        d.recompute();
                        d.invalidate();
                    });
                }
                LRESULT(1)
            }
            WM_DASH_RELOAD => {
                with(|d| {
                    d.load_history();
                    d.invalidate();
                });
                LRESULT(0)
            }
            WM_DASH_TAB => {
                if IsIconic(hwnd).as_bool() {
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                }
                let _ = SetForegroundWindow(hwnd);
                with(|d| d.set_tab(wp.0.min(2)));
                LRESULT(0)
            }
            WM_SETTINGCHANGE => {
                with(|d| {
                    let t = Theme::current();
                    if t.dark != d.theme.dark {
                        d.theme = t;
                        gui::style_frame(hwnd, &t);
                        d.invalidate();
                    }
                });
                LRESULT(0)
            }
            WM_DPICHANGED => {
                let r = &*(lp.0 as *const RECT);
                with(|d| {
                    d.s = (wp.0 & 0xFFFF) as f32 / 96.0;
                    d.fonts.clear();
                    d.underline = Tween::new(0.0);
                    d.base = None;
                    d.dirty = true;
                });
                let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                LRESULT(0)
            }
            WM_SIZE => {
                with(|d| {
                    d.underline = Tween::new(0.0);
                    d.dirty = true;
                });
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_GETMINMAXINFO => {
                let mmi = &mut *(lp.0 as *mut MINMAXINFO);
                let s = GetDpiForWindow(hwnd).max(96) as i32;
                mmi.ptMinTrackSize = POINT { x: 820 * s / 96, y: 720 * s / 96 };
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

fn set_title(hwnd: HWND, name: &str) {
    unsafe {
        let _ = SetWindowTextW(hwnd, &HSTRING::from(format!("{name} - BatteryHub")));
    }
}

/// `--dashboard <tab> <device>`: runs the dashboard window until it is closed.
pub fn run_standalone(args: &[String]) {
    let tab: usize = args.first().and_then(|t| t.parse().ok()).unwrap_or(0).min(2);
    let Some((status, reported)) = args.get(1).and_then(|s| decode_status(s)) else { return };
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: LoadIconW(Some(hinst.into()), PCWSTR(std::ptr::dangling::<u16>())).unwrap_or_default(),
            lpszClassName: CLASS,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        // Centred on the monitor under the mouse, 900 x 760 at 100 %.
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(mon, &mut mi);
        let mut dpi_x = 96u32;
        let mut dpi_y = 96u32;
        let _ = windows::Win32::UI::HiDpi::GetDpiForMonitor(mon, windows::Win32::UI::HiDpi::MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
        let s = dpi_x as f32 / 96.0;
        let work = mi.rcWork;
        let (w, h) = (((900.0 * s) as i32).min(work.right - work.left), ((780.0 * s) as i32).min(work.bottom - work.top));
        let (x, y) = (work.left + (work.right - work.left - w) / 2, work.top + (work.bottom - work.top - h) / 2);
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            CLASS,
            &HSTRING::from(format!("{} - BatteryHub", status.name)),
            WS_OVERLAPPEDWINDOW,
            x,
            y,
            w,
            h,
            None,
            None,
            Some(hinst.into()),
            None,
        ) else {
            return;
        };
        let id = crate::util::fnv1a(&status.key) as usize | 1;
        let _ = SetPropW(hwnd, KEY_PROP, Some(windows::Win32::Foundation::HANDLE(id as *mut c_void)));
        let theme = Theme::current();
        gui::style_frame(hwnd, &theme);
        let s = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
        let mut d = Dash {
            hwnd,
            theme,
            s,
            fonts: Fonts::default(),
            base: None,
            dirty: true,
            overlay: None,
            status,
            reported,
            series: Vec::new(),
            stats: Stats::default(),
            tab,
            range: 1,
            underline: Tween::new(0.0),
            underline_w: Tween::new(0.0),
            reveal: Tween::new(0.0),
            hits: Hits::default(),
            hover: None,
            tracking: false,
            chart: None,
            mouse: None,
        };
        d.load_history();
        d.reveal.go(1.0, 550.0);
        DASH.with(|c| *c.borrow_mut() = Some(d));
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        SetTimer(Some(hwnd), TIMER_ANIM, 15, None);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = RemovePropW(hwnd, KEY_PROP);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_round_trip() {
        let s = DeviceStatus {
            key: "logi:unit:ABC".into(),
            name: "Office \"mouse\"".into(),
            via: "2.4 GHz receiver".into(),
            kind: DeviceKind::Mouse,
            percent: 42,
            detail: "L 80%, R 75%".into(),
            charging: true,
            millivolts: 3900,
            hw: Some((0x046D, 0xC547)),
            ..Default::default()
        };
        let (d, reported) = decode_status(&encode_status(&s, "PRO 2 LIGHTSPEED")).unwrap();
        assert_eq!(reported, "PRO 2 LIGHTSPEED");
        assert_eq!((d.key, d.name, d.kind, d.percent, d.charging, d.hw, d.millivolts), (s.key, s.name, s.kind, 42, true, s.hw, 3900));
        assert!(decode_status("").is_none());
        assert!(decode_status("a\u{1F}b").is_none());
    }

    #[test]
    fn time_texts() {
        let now = 1_795_300_000;
        assert_eq!(fmt_ago(now - 30, now), "just now");
        assert_eq!(fmt_ago(now - 600, now), "10 min ago");
        assert_eq!(fmt_ago(now - 5 * 3600, now), "5 h ago");
        assert_eq!(fmt_ago(now - 3 * 86_400, now), "3 d ago");
        let m = local_midnight(now);
        assert!(m <= now && now - m < 86_400 + 3600);
        let l = local(m);
        assert_eq!((l.hour, l.minute, l.second), (0, 0, 0));
        assert_eq!(fmt_when(m + 3600 * 14 + 120, m + 3600 * 15), "14:02");
    }
}

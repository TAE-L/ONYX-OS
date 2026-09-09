//! M8-quickwin: CMOS RTC driver + basic kernel timekeeping.
//!
//! Reads the wall clock from the CMOS/RTC (ports 0x70/0x71) once at boot:
//!   * waits out the update-in-progress flag (status A bit 7),
//!   * reads the date/time fields twice and retries until both reads agree,
//!   * decodes BCD (status B bit 2) and 12-hour (status B bit 1) formats,
//!     including the optional century register (0x32).
//! No RTC periodic/IRQ usage — everything else derives from the PIT:
//! current time of day = boot time-of-day + `pit::ticks()`.
//!
//! The clock is displayed in the framebuffer header bar by the `clock`
//! task (see `main.rs`) via `framebuffer::draw_clock`.

use alloc::format;
use core::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use spin::Mutex;

use x86_64::instructions::port::Port;

use crate::{pit, serial_writeln};

/// A calendar date + time of day as read from the RTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateTime {
    pub second: u8,
    pub minute: u8,
    /// 24-hour format (0..23), converted if the RTC reports 12-hour time.
    pub hour: u8,
    pub day: u8,
    pub month: u8,
    /// Full year (e.g. 2026).
    pub year: u16,
}

impl DateTime {
    const ZEROED: DateTime = DateTime {
        second: 0,
        minute: 0,
        hour: 0,
        day: 0,
        month: 0,
        year: 0,
    };

    /// Seconds since midnight (for the header clock).
    pub fn secs_of_day(&self) -> u32 {
        u32::from(self.hour) * 3600 + u32::from(self.minute) * 60 + u32::from(self.second)
    }
}

/// Wall-clock timestamp captured at boot (written once by `init`).
static BOOT_TIME: Mutex<DateTime> = Mutex::new(DateTime::ZEROED);

/// Sanity flag for the clock task: boot RTC read succeeded.
static RTC_OK: AtomicU32 = AtomicU32::new(0);

/// Display timezone offset from UTC, in seconds. Default: IST (UTC+5:30).
/// The RTC is always interpreted as UTC; this offset produces the time shown
/// on screen. QOL-update hook: a future settings feature can call
/// `set_tz_offset_secs` at boot (e.g. from a config file on the FAT volume)
/// so each installation gets its own zone.
static TZ_OFFSET_SECS: AtomicI32 = AtomicI32::new(5 * 3600 + 30 * 60);

/// Change the display timezone (seconds east of UTC). Call before `init`
/// (or any time — the next tick redraws with the new zone).
pub fn set_tz_offset_secs(secs: i32) {
    TZ_OFFSET_SECS.store(secs, Ordering::Relaxed);
}

/// CMOS address/data port handles.
struct Cmos {
    addr: Port<u8>,
    data: Port<u8>,
}

impl Cmos {
    const fn new() -> Self {
        Self {
            addr: Port::new(0x70),
            data: Port::new(0x71),
        }
    }

    /// Select a CMOS register (bit 7 = keep NMI masked; we never unmask it).
    unsafe fn select(&mut self, reg: u8) {
        self.addr.write(reg | 0x80);
    }

    unsafe fn read_reg(&mut self, reg: u8) -> u8 {
        self.select(reg);
        self.data.read()
    }
}

/// True while the RTC is internally updating its registers (status A bit 7).
/// Bounded by a spin budget so a weird virtual RTC can't hang us forever.
unsafe fn update_in_progress(cmos: &mut Cmos) -> bool {
    for _ in 0..1_000_000 {
        if cmos.read_reg(0x0A) & 0x80 == 0 {
            return false;
        }
    }
    false // timed out: proceed and let the double-read consistency check win
}

/// Read one raw RTC field (waiting out any in-progress update first).
unsafe fn read_field(cmos: &mut Cmos, reg: u8) -> u8 {
    update_in_progress(cmos);
    cmos.read_reg(reg)
}

/// Read the full date/time twice, retrying until both snapshots agree
/// (guards against a register rollover mid-read).
unsafe fn read_datetime_stable(cmos: &mut Cmos, status_b: u8) -> Option<DateTime> {
    for _ in 0..10 {
        let a = raw_snapshot(cmos);
        let b = raw_snapshot(cmos);
        if a == b {
            return Some(decode(a, status_b));
        }
    }
    None
}

unsafe fn raw_snapshot(cmos: &mut Cmos) -> [u8; 7] {
    [
        read_field(cmos, 0x00), // second
        read_field(cmos, 0x02), // minute
        read_field(cmos, 0x04), // hour
        read_field(cmos, 0x07), // day
        read_field(cmos, 0x08), // month
        read_field(cmos, 0x09), // year (0..99)
        read_field(cmos, 0x32), // century (0 if unsupported)
    ]
}

/// Apply BCD / 12-hour / century decoding per status B.
fn decode(raw: [u8; 7], status_b: u8) -> DateTime {
    let bcd = status_b & 0x04 == 0;
    let d = |v: u8| {
        if bcd {
            (v & 0x0F) + (v >> 4) * 10
        } else {
            v
        }
    };
    // 12-hour mode: bit 7 of the raw hour = PM; noon/midnight special cases.
    let hour = if status_b & 0x02 == 0 {
        let h12 = d(raw[2] & 0x7F);
        let pm = raw[2] & 0x80 != 0;
        match (pm, h12) {
            (true, 12) => 12,
            (true, h) => h + 12,
            (false, 12) => 0,
            (false, h) => h,
        }
    } else {
        d(raw[2])
    };
    let year_raw = u16::from(d(raw[5]));
    let year = if raw[6] >= 19 {
        u16::from(d(raw[6])) * 100 + year_raw
    } else {
        2000 + year_raw // no century register: assume 2000s
    };
    DateTime {
        second: d(raw[0]),
        minute: d(raw[1]),
        hour,
        day: d(raw[3]),
        month: d(raw[4]),
        year,
    }
}

/// Apply the display timezone offset to a timestamp, handling the midnight
/// date rollover (offsets are within ±14 h, so at most one day step occurs).
fn to_local(dt: DateTime) -> DateTime {
    let off = i64::from(TZ_OFFSET_SECS.load(Ordering::Relaxed));
    let total = i64::from(dt.secs_of_day()) + off;
    let day_shift = total.div_euclid(86_400);
    let sod = total.rem_euclid(86_400) as u32;
    let mut d = dt;
    d.second = (sod % 60) as u8;
    d.minute = ((sod / 60) % 60) as u8;
    d.hour = (sod / 3600) as u8;
    if day_shift > 0 {
        for _ in 0..day_shift {
            d = next_day(d);
        }
    } else if day_shift < 0 {
        for _ in 0..(-day_shift) {
            d = prev_day(d);
        }
    }
    d
}

fn is_leap(y: u16) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(year) {
                29
            } else {
                28
            }
        }
        _ => 30, // implausible month: stay benign rather than panic
    }
}

fn next_day(mut d: DateTime) -> DateTime {
    if d.day >= days_in_month(d.year, d.month) {
        if d.month == 12 {
            d.year += 1;
            d.month = 1;
        } else {
            d.month += 1;
        }
        d.day = 1;
    } else {
        d.day += 1;
    }
    d
}

fn prev_day(mut d: DateTime) -> DateTime {
    if d.day > 1 {
        d.day -= 1;
        return d;
    }
    if d.month == 1 {
        d.year -= 1;
        d.month = 12;
    } else {
        d.month -= 1;
    }
    d.day = days_in_month(d.year, d.month);
    d
}

/// Read the RTC, record the boot timestamp, log it. Call once at boot.
pub fn init() {
    let mut cmos = Cmos::new();
    let status_b = unsafe { cmos.read_reg(0x0B) };
    let dt = unsafe { read_datetime_stable(&mut cmos, status_b) };
    match dt {
        Some(dt) if (1..=12).contains(&dt.month) && (1..=31).contains(&dt.day) => {
            // Store UTC as the base timestamp; the display offset is applied
            // at read time (`to_local`), so a runtime `set_tz_offset_secs`
            // takes effect on the very next clock tick.
            *BOOT_TIME.lock() = dt;
            RTC_OK.store(1, Ordering::Relaxed);
            let local = to_local(dt);
            serial_writeln!(
                "rtc: boot UTC = {:04}-{:02}-{:02} {:02}:{:02}:{:02} -> local = {:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                dt.year,
                dt.month,
                dt.day,
                dt.hour,
                dt.minute,
                dt.second,
                local.year,
                local.month,
                local.day,
                local.hour,
                local.minute,
                local.second
            );
        }
        _ => {
            serial_writeln!("rtc: CMOS read failed/implausible — clock disabled");
        }
    }
}

/// Did `init` get a plausible timestamp?
pub fn available() -> bool {
    RTC_OK.load(Ordering::Relaxed) == 1
}

/// The boot timestamp in UTC (base for all derived times).
pub fn boot_datetime_utc() -> DateTime {
    *BOOT_TIME.lock()
}

/// Current wall clock in UTC: boot RTC advanced by PIT uptime, with the date
/// advanced across midnight rollovers (day-accurate for long uptimes).
pub fn now_utc() -> DateTime {
    let mut dt = boot_datetime_utc();
    let up_secs = (pit::ticks() / u64::from(pit::TICK_HZ)) as i64;
    let sod = i64::from(dt.secs_of_day()) + up_secs;
    let day_shift = sod.div_euclid(86_400);
    let rest = sod.rem_euclid(86_400) as u32;
    dt.second = (rest % 60) as u8;
    dt.minute = ((rest / 60) % 60) as u8;
    dt.hour = (rest / 3600) as u8;
    for _ in 0..day_shift {
        dt = next_day(dt);
    }
    dt
}

/// Current wall clock in the display timezone. The RTC is always UTC; the
/// offset is applied here at read time, so `set_tz_offset_secs` works at any
/// moment (not just before boot).
pub fn now() -> DateTime {
    to_local(now_utc())
}

/// Current time-of-day in seconds (display zone). Falls back to raw
/// uptime-as-clock if the RTC was unusable.
pub fn now_secs_of_day() -> u32 {
    if available() {
        now().secs_of_day()
    } else {
        ((pit::ticks() / u64::from(pit::TICK_HZ)) % 86_400) as u32
    }
}

/// Format seconds-of-day as `HH:MM:SS` (24 h).
pub fn format_hms(secs_of_day: u32) -> alloc::string::String {
    let h = secs_of_day / 3600;
    let m = (secs_of_day % 3600) / 60;
    let s = secs_of_day % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}

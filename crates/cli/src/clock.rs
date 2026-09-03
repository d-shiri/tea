//! Local wall-clock time: what hour it is, and what day.
//!
//! The scheduler measures durations and never asks what o'clock it is -- that
//! is what keeps it testable. But two things here do care: working hours, which
//! are a time of day, and the day's tally, which has to know when midnight
//! happened. Both want *local* time including whatever the timezone is doing
//! this month, which is a database question rather than an arithmetic one, so
//! it is asked of glib rather than worked out from a Unix timestamp here.

use gtk::glib;
use serde::{Deserialize, de};
use std::fmt;

/// Minutes since local midnight, and the day of the week.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Now {
    /// 0 at midnight, 1439 at 23:59.
    pub minute: u32,
    /// Monday is 0, Sunday is 6.
    pub weekday: u32,
}

pub fn now() -> Now {
    match glib::DateTime::now_local() {
        Ok(t) => Now {
            minute: (t.hour().max(0) as u32) * 60 + t.minute().max(0) as u32,
            // glib counts Monday as 1; everything here counts from zero.
            weekday: (t.day_of_week().max(1) as u32 - 1) % 7,
        },
        // No timezone to be had. Rather than guess an hour and quietly stop
        // breaking someone at the wrong time, answer with a moment that is
        // inside every window anybody would write: the middle of a Wednesday.
        Err(_) => Now { minute: 12 * 60, weekday: 2 },
    }
}

/// Today as `2026-09-01`, for the tally that resets at midnight.
///
/// A string rather than a number because it is only ever compared with the one
/// written yesterday, and a date that can be read in the state file is worth
/// more than a date that saves six bytes.
pub fn today() -> String {
    match glib::DateTime::now_local() {
        Ok(t) => format!("{:04}-{:02}-{:02}", t.year(), t.month(), t.day_of_month()),
        Err(_) => "unknown".to_string(),
    }
}

/// The local date, as [`today`] writes it, of an instant written the way
/// Home Assistant writes one -- `2026-09-03T07:21:05.222188+00:00`. `None`
/// for anything that does not read as a moment at all.
///
/// Local rather than UTC on purpose: a job ticked off at one in the morning
/// is tonight's job, not tomorrow's, and the day line under the list is read
/// by somebody standing in a kitchen, not at Greenwich.
pub fn day_of(iso: &str) -> Option<String> {
    let t = glib::DateTime::from_iso8601(iso.trim(), None).ok()?.to_local().ok()?;
    Some(format!("{:04}-{:02}-{:02}", t.year(), t.month(), t.day_of_month()))
}

/// `17:30` for a moment given as minutes since midnight.
pub fn oclock(minute: u32) -> String {
    format!("{:02}:{:02}", (minute / 60) % 24, minute % 60)
}

/// Unix seconds now, for the one thing that has to survive a restart: how long
/// `tea off` has left to run.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A local time of day written `"09:00"`, or `"off"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clock(pub Option<u32>);

impl Clock {
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.is_empty() || s.eq_ignore_ascii_case("off") || s.eq_ignore_ascii_case("always") {
            return Some(Self(None));
        }
        let (h, m) = s.split_once(':')?;
        let h: u32 = h.trim().parse().ok()?;
        let m: u32 = m.trim().parse().ok()?;
        // 24:00 is a legitimate way to write "the end of the day", and the only
        // value above 23:59 anybody means on purpose.
        if h > 24 || m > 59 || (h == 24 && m > 0) {
            return None;
        }
        Some(Self(Some(h * 60 + m)))
    }

    pub fn minute(&self) -> Option<u32> {
        self.0
    }
}

impl std::fmt::Display for Clock {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self.0 {
            Some(m) => f.write_str(&oclock(m)),
            None => f.write_str("off"),
        }
    }
}

impl<'de> Deserialize<'de> for Clock {
    fn deserialize<D: de::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Clock::parse(&raw).ok_or_else(|| {
            de::Error::custom(format!("{raw:?} is not a time of day like \"09:00\", or \"off\""))
        })
    }
}

/// Which days of the week a thing applies to.
///
/// A bitmask rather than a list because the only question ever asked of it is
/// "does it include today", and because it makes "mon-fri" and "mon,tue,wed,
/// thu,fri" the same value rather than two spellings to keep straight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Days(u8);

const DAY_NAMES: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

impl Days {
    pub fn all() -> Self {
        Self(0b111_1111)
    }

    pub fn includes(&self, weekday: u32) -> bool {
        self.0 & (1 << (weekday % 7)) != 0
    }

    pub fn every_day(&self) -> bool {
        *self == Self::all()
    }

    /// `"all"`, `"mon-fri"`, `"mon,wed,fri"`, or any mixture of names and
    /// ranges. Case and spaces are not worth being strict about; a day name
    /// that is not a day is, because "tues" silently meaning nothing would be
    /// a week with a hole in it.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        if s.is_empty() || s == "all" || s == "every day" || s == "daily" {
            return Some(Self::all());
        }
        if s == "none" || s == "off" {
            return Some(Self(0));
        }
        let mut mask = 0u8;
        for part in s.split(',') {
            let part = part.trim();
            match part.split_once('-') {
                Some((from, to)) => {
                    let (from, to) = (index(from)?, index(to)?);
                    // Wrapping ranges are the point of writing one: "sat-sun"
                    // is a weekend, and so is "sun-sat" to somebody who counts
                    // the week from Sunday.
                    let mut day = from;
                    loop {
                        mask |= 1 << day;
                        if day == to {
                            break;
                        }
                        day = (day + 1) % 7;
                    }
                }
                None => mask |= 1 << index(part)?,
            }
        }
        Some(Self(mask))
    }
}

/// The three-letter name or the whole word, and nothing in between: "tues"
/// looks like a Tuesday and is not one, and a config that quietly drops a day
/// is a day tea never breaks you on for reasons you will never find.
fn index(name: &str) -> Option<u32> {
    const WHOLE: [&str; 7] =
        ["monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"];
    let name = name.trim();
    DAY_NAMES
        .iter()
        .zip(WHOLE)
        .position(|(short, whole)| name == *short || name == whole)
        .map(|i| i as u32)
}

impl fmt::Display for Days {
    /// Runs of three or more collapse to a range, because "mon–fri" is a week
    /// and "mon, tue, wed, thu, fri" is a list somebody has to read to the end
    /// of before they know it is the ordinary one.
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.every_day() {
            return f.write_str("every day");
        }
        if self.0 == 0 {
            return f.write_str("never");
        }
        let mut parts: Vec<String> = Vec::new();
        let mut day = 0;
        while day < 7 {
            if !self.includes(day) {
                day += 1;
                continue;
            }
            let start = day;
            while day + 1 < 7 && self.includes(day + 1) {
                day += 1;
            }
            match day - start {
                0 => parts.push(DAY_NAMES[start as usize].to_string()),
                1 => {
                    parts.push(DAY_NAMES[start as usize].to_string());
                    parts.push(DAY_NAMES[day as usize].to_string());
                }
                _ => parts
                    .push(format!("{}–{}", DAY_NAMES[start as usize], DAY_NAMES[day as usize])),
            }
            day += 1;
        }
        f.write_str(&parts.join(", "))
    }
}

impl<'de> Deserialize<'de> for Days {
    fn deserialize<D: de::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Days::parse(&raw).ok_or_else(|| {
            de::Error::custom(format!("{raw:?} is not days like \"mon-fri\", or \"all\""))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stamp_from_the_hub_has_a_day() {
        // Noon UTC is the same date in every timezone anybody lives in.
        assert_eq!(day_of("2026-09-03T12:00:00+00:00").as_deref(), Some("2026-09-03"));
        assert_eq!(day_of("2026-09-03T12:00:00.222188+00:00").as_deref(), Some("2026-09-03"));
        assert_eq!(day_of(""), None);
        assert_eq!(day_of("yesterday"), None);
    }

    #[test]
    fn a_time_of_day_reads_the_way_it_is_written() {
        assert_eq!(Clock::parse("09:00").unwrap().minute(), Some(540));
        assert_eq!(Clock::parse(" 23:59 ").unwrap().minute(), Some(1439));
        assert_eq!(Clock::parse("24:00").unwrap().minute(), Some(1440));
        // Unset, and the two words people write when they mean unset.
        assert_eq!(Clock::parse("").unwrap().minute(), None);
        assert_eq!(Clock::parse("off").unwrap().minute(), None);
        assert_eq!(Clock::parse("always").unwrap().minute(), None);
        // Anything that is not a time at all has to be refused rather than
        // guessed at: silently reading "9am" as midnight would switch tea off
        // for the working day.
        for bad in ["9am", "9", "25:00", "09:60", "half nine", ":", "09:"] {
            assert!(Clock::parse(bad).is_none(), "{bad:?} is not a time of day");
        }
    }

    #[test]
    fn days_read_as_names_ranges_or_the_lot() {
        let mon_fri = Days::parse("mon-fri").unwrap();
        for day in 0..5 {
            assert!(mon_fri.includes(day), "weekday {day} is a working day");
        }
        assert!(!mon_fri.includes(5) && !mon_fri.includes(6), "the weekend is not");
        assert_eq!(Days::parse("mon,tue,wed,thu,fri").unwrap(), mon_fri, "same week, spelt out");
        assert_eq!(Days::parse("Monday - Friday").unwrap(), mon_fri, "and written out");

        // A range that wraps the end of the week is the point of writing one.
        let weekend = Days::parse("sat-sun").unwrap();
        assert!(weekend.includes(5) && weekend.includes(6) && !weekend.includes(0));
        assert!(Days::parse("fri-mon").unwrap().includes(0), "fri-mon reaches Monday");

        assert!(Days::parse("all").unwrap().every_day());
        assert!(Days::parse("").unwrap().every_day());
        assert_eq!(mon_fri.to_string(), "mon–fri", "a week is a range, not a list");
        assert_eq!(weekend.to_string(), "sat, sun", "and two days are just two days");
        assert_eq!(Days::parse("mon,wed,fri").unwrap().to_string(), "mon, wed, fri");

        // A day that is not a day has to be refused: a silent hole in the week
        // is a Tuesday tea never breaks you on and you never find out why.
        for bad in ["tues", "mon-funday", "weekdays", "1-5"] {
            assert!(Days::parse(bad).is_none(), "{bad:?} is not a set of days");
        }
    }

    #[test]
    fn a_clock_prints_the_way_it_was_written() {
        assert_eq!(Clock::parse("09:05").unwrap().to_string(), "09:05");
        assert_eq!(Clock::parse("off").unwrap().to_string(), "off");
        assert_eq!(oclock(1440), "00:00", "midnight the next day is still midnight");
    }
}

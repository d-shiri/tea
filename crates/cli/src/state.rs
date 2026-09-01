//! Break debt that survives a restart.
//!
//! The interesting question is not how to write the file, it is what to do
//! about the time tea was not running. The rule:
//!
//! - **The machine was up the whole time** — tea was stopped, killed or
//!   crashed while you kept working. That gap counts as *work*. Otherwise
//!   `systemctl --user restart tea` is a one-command dodge, which is exactly
//!   what persisting state is supposed to prevent.
//! - **The machine rebooted** — you were not at the keyboard for at least part
//!   of it. That gap counts as *rest*, and the normal idle-credit rules decide
//!   whether it was long enough to clear the debt.
//!
//! Reboot is detected by boottime going backwards, which is the one thing a
//! reboot cannot fake.

use tea_core::Snapshot;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const VERSION: u32 = 1;
/// Bound on how much progress a `kill -9` can cost. Writing every tick would
/// mean 86,400 writes a day to save a number nobody reads.
const WRITE_EVERY: Duration = Duration::from_secs(5);

#[derive(Serialize, Deserialize)]
struct Saved {
    version: u32,
    saved_unix: u64,
    saved_boottime: u64,
    breaking: bool,
    worked: u64,
    rested: u64,
    due: bool,
    postpones_used: u32,
    window_elapsed: u64,
    /// Both default, so a state file written before the tag existed still
    /// loads. Missing them would only ever mean "no scan yet", which is the
    /// safe reading anyway.
    #[serde(default)]
    released: bool,
    #[serde(default)]
    waiting: u64,
    /// Breaks begun, which is what decides when the next long one falls due,
    /// and how long the one in progress runs for. Defaulted like the rest: a
    /// state file written before long breaks existed reads as "none yet".
    #[serde(default)]
    breaks_done: u32,
    #[serde(default)]
    break_len: u64,
    /// The day's tally, and the day it belongs to. Kept here rather than in a
    /// file of its own because it is written on exactly the same occasions and
    /// two files would only be two chances to disagree.
    #[serde(default)]
    day: String,
    #[serde(default)]
    breaks_today: u32,
    #[serde(default)]
    postponed_today: u32,
    #[serde(default)]
    credited_today: u32,
    #[serde(default)]
    steps_today: u32,
}

/// What today came to: how much of this actually worked.
///
/// Counted by the host rather than the scheduler, because the interesting
/// numbers are not all scheduling ones -- steps walked comes from the hub, and
/// postpones are a thing you did rather than a state the timer was in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tally {
    /// The local date these numbers belong to, `2026-09-01`.
    pub day: String,
    /// Breaks that ran and ended.
    pub breaks: u32,
    /// Postpones granted.
    pub postponed: u32,
    /// Breaks paid for by being away from the keyboard rather than by a page.
    pub credited: u32,
    /// Steps walked during breaks, where a walk is counted at all.
    pub steps: u32,
}

impl Tally {
    /// Start again at midnight. Yesterday's four breaks are not today's, and a
    /// tally that never rolls over is a number that only ever goes up and
    /// stops meaning anything by Thursday.
    pub fn roll(&mut self, today: &str) {
        if self.day != today {
            *self = Self { day: today.to_string(), ..Self::default() };
        }
    }

    /// Whether anything happened at all today.
    pub fn quiet(&self) -> bool {
        self.breaks == 0 && self.postponed == 0 && self.credited == 0
    }
}

/// How much of a gap the machine spent switched off, and therefore counts as
/// rest rather than as work done while tea was not running.
///
/// Boottime keeps counting through suspend but restarts from zero on a reboot,
/// so whatever it cannot account for is time the machine was not on. Comparing
/// the two clocks catches the case a simple `boottime went backwards` test
/// misses: a machine that has now been up *longer* than it had been when the
/// state was written still rebooted in between.
fn time_switched_off(
    gap: Duration,
    boottime_now: Duration,
    boottime_saved: Duration,
) -> Duration {
    let awake = boottime_now.saturating_sub(boottime_saved);
    gap.saturating_sub(awake)
}

/// A restored snapshot plus the catch-up tick that accounts for the downtime.
pub struct Restored {
    pub snapshot: Snapshot,
    pub gap: Duration,
    pub idle: Duration,
    pub tally: Tally,
}

pub struct Store {
    path: PathBuf,
    last_write: Option<Duration>,
    complained: bool,
}

/// `$XDG_STATE_HOME/tea/`, else `~/.local/state/tea/`.
fn dir() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_STATE_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".local").join("state"),
    };
    Some(base.join("tea"))
}

/// The off switch: `tea off 1h`.
///
/// A file of its own rather than a field in the state file, because the state
/// file belongs to the daemon -- it rewrites it every few seconds -- and a
/// second process writing into it would have its answer overwritten before
/// anybody read it. This one is written by the command and only ever read by
/// the daemon, which is the whole of the protocol.
pub mod off {
    use super::dir;
    use serde::{Deserialize, Serialize};
    use std::path::PathBuf;

    #[derive(Serialize, Deserialize)]
    struct Until {
        /// Unix seconds. Wall clock rather than boottime on purpose: "back at
        /// four" has to survive a suspend, and a laptop shut for the afternoon
        /// should wake up with the afternoon over.
        until_unix: u64,
    }

    fn path() -> Option<PathBuf> {
        dir().map(|d| d.join("off.toml"))
    }

    /// Seconds of peace remaining, or `None` if tea is on.
    pub fn left() -> Option<std::time::Duration> {
        let text = std::fs::read_to_string(path()?).ok()?;
        let until: Until = toml::from_str(&text).ok()?;
        let now = crate::clock::unix_now();
        // Expired is the same as absent, and the file is left to be tidied up
        // by whoever writes the next one: a daemon that deletes files it did
        // not create is a daemon that deletes the wrong file eventually.
        match until.until_unix > now {
            true => Some(std::time::Duration::from_secs(until.until_unix - now)),
            false => None,
        }
    }

    /// Switch off for `how_long`.
    pub fn set(how_long: std::time::Duration) -> Result<(), String> {
        let path = path().ok_or("no state directory to write to")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        let until = Until { until_unix: crate::clock::unix_now() + how_long.as_secs() };
        let text = toml::to_string(&until).map_err(|e| e.to_string())?;
        std::fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
    }

    /// Back on, now.
    pub fn clear() -> Result<(), String> {
        let Some(path) = path() else {
            return Ok(());
        };
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            // Already on is not a failure to turn on.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("cannot remove {}: {e}", path.display())),
        }
    }
}

impl Store {
    /// `$XDG_STATE_HOME/tea/state.toml`, else `~/.local/state/...`.
    pub fn new() -> Option<Self> {
        Some(Self { path: dir()?.join("state.toml"), last_write: None, complained: false })
    }

    pub fn load(&self, boottime_now: Duration) -> Option<Restored> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        let saved: Saved = match toml::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("tea: ignoring unreadable state file ({e})");
                return None;
            }
        };
        if saved.version != VERSION {
            return None;
        }

        let now_unix = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        // A backwards wall clock (NTP step, timezone fiddling) must not hand out
        // free rest, so anything negative is simply no gap at all.
        let gap = Duration::from_secs(now_unix.saturating_sub(saved.saved_unix));

        let idle = time_switched_off(gap, boottime_now, Duration::from_secs(saved.saved_boottime));

        Some(Restored {
            snapshot: Snapshot {
                breaking: saved.breaking,
                worked: Duration::from_secs(saved.worked),
                rested: Duration::from_secs(saved.rested),
                due: saved.due,
                postpones_used: saved.postpones_used,
                window_elapsed: Duration::from_secs(saved.window_elapsed),
                released: saved.released,
                waiting: Duration::from_secs(saved.waiting),
                breaks_done: saved.breaks_done,
                break_len: Duration::from_secs(saved.break_len),
            },
            gap,
            idle,
            tally: Tally {
                day: saved.day,
                breaks: saved.breaks_today,
                postponed: saved.postponed_today,
                credited: saved.credited_today,
                steps: saved.steps_today,
            },
        })
    }

    /// `force` for state changes worth not losing; otherwise throttled.
    pub fn save(&mut self, snap: Snapshot, tally: &Tally, boottime_now: Duration, force: bool) {
        if !force && self.last_write.is_some_and(|t| boottime_now < t + WRITE_EVERY) {
            return;
        }
        self.last_write = Some(boottime_now);

        let Ok(now_unix) = SystemTime::now().duration_since(UNIX_EPOCH) else {
            return;
        };
        let saved = Saved {
            version: VERSION,
            saved_unix: now_unix.as_secs(),
            saved_boottime: boottime_now.as_secs(),
            breaking: snap.breaking,
            worked: snap.worked.as_secs(),
            rested: snap.rested.as_secs(),
            due: snap.due,
            postpones_used: snap.postpones_used,
            window_elapsed: snap.window_elapsed.as_secs(),
            released: snap.released,
            waiting: snap.waiting.as_secs(),
            breaks_done: snap.breaks_done,
            break_len: snap.break_len.as_secs(),
            day: tally.day.clone(),
            breaks_today: tally.breaks,
            postponed_today: tally.postponed,
            credited_today: tally.credited,
            steps_today: tally.steps,
        };

        if let Err(e) = self.write(&saved) {
            if !self.complained {
                self.complained = true;
                eprintln!("tea: cannot save state ({e}); a restart will forget your progress");
            }
        }
    }

    /// Write to a temporary file and rename over the target, so an interrupted
    /// write leaves the previous state intact rather than a truncated file.
    fn write(&self, saved: &Saved) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string(saved)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn a_stopped_process_on_a_running_machine_is_work() {
        // Gone ten minutes, but the machine was up the whole time.
        let off = time_switched_off(secs(600), secs(4000), secs(3400));
        assert_eq!(off, Duration::ZERO, "restarting tea must not buy rest");
    }

    #[test]
    fn an_overnight_shutdown_is_rest() {
        // Saved after two minutes of uptime, then off for eight hours, then
        // booted again -- and tea started a minute and a half in.
        let off = time_switched_off(secs(28_800), secs(90), secs(120));
        assert_eq!(off, secs(28_800));
    }

    #[test]
    fn a_reboot_into_longer_uptime_is_still_rest() {
        // The case a "boottime went backwards" test gets wrong: the new boot
        // has already been up longer than the old one had when it saved.
        let off = time_switched_off(secs(28_800), secs(600), secs(120));
        assert_eq!(off, secs(28_320), "only the 8 minutes since boot were awake");
    }

    #[test]
    fn the_tally_starts_again_at_midnight() {
        let mut day = Tally { day: "2026-08-31".into(), breaks: 4, postponed: 2, steps: 300, ..Tally::default() };
        day.roll("2026-08-31");
        assert_eq!(day.breaks, 4, "the same day keeps its numbers");

        day.roll("2026-09-01");
        assert_eq!(day, Tally { day: "2026-09-01".into(), ..Tally::default() });
        assert!(day.quiet());
    }

    #[test]
    fn a_backwards_wall_clock_hands_out_nothing() {
        assert_eq!(time_switched_off(Duration::ZERO, secs(90), secs(120)), Duration::ZERO);
    }
}

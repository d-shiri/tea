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
}

pub struct Store {
    path: PathBuf,
    last_write: Option<Duration>,
    complained: bool,
}

impl Store {
    /// `$XDG_STATE_HOME/tea/state.toml`, else `~/.local/state/...`.
    pub fn new() -> Option<Self> {
        let base = match std::env::var_os("XDG_STATE_HOME") {
            Some(v) if !v.is_empty() => PathBuf::from(v),
            _ => PathBuf::from(std::env::var_os("HOME")?).join(".local").join("state"),
        };
        Some(Self {
            path: base.join("tea").join("state.toml"),
            last_write: None,
            complained: false,
        })
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
            },
            gap,
            idle,
        })
    }

    /// `force` for state changes worth not losing; otherwise throttled.
    pub fn save(&mut self, snap: Snapshot, boottime_now: Duration, force: bool) {
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
    fn a_backwards_wall_clock_hands_out_nothing() {
        assert_eq!(time_switched_off(Duration::ZERO, secs(90), secs(120)), Duration::ZERO);
    }
}

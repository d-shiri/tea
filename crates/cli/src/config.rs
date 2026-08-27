//! Config file loading. Defaults < config.toml < command-line flags.

use serde::{Deserialize, de};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DEFAULT_FILE: &str = r#"# tea — take a break, whether you like it or not
#
# Durations are strings: "90s", "25m", "1h". A bare number means minutes.
# Command-line flags override anything set here.

work = "25m"          # how long you work before a break falls due
break = "5m"          # how long the break lasts
warn_before = "30s"   # heads-up before the overlay appears

[idle]
# Away this long and it counts as the break you were owed. Being ambushed the
# moment you sit back down is how this kind of tool gets uninstalled.
credit = "5m"
# Away this long and the work timer stops banking time you didn't spend.
pause = "1m"

[postpone]
duration = "1m"       # how much time one postpone buys
budget = 2            # postpones allowed per window (0 disables postponing)
window = "1h"
"#;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FileConfig {
    pub work: Dur,
    #[serde(rename = "break")]
    pub brk: Dur,
    pub warn_before: Dur,
    pub idle: Idle,
    pub postpone: Postpone,
    pub calls: Calls,
    pub sound: crate::sound::Config,
    pub animation: crate::overlay::Anim,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Idle {
    pub credit: Dur,
    pub pause: Dur,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Calls {
    pub warn_after: Dur,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Postpone {
    pub duration: Dur,
    pub budget: u32,
    pub window: Dur,
}

impl Default for FileConfig {
    fn default() -> Self {
        let d = tea_core::Config::default();
        Self {
            work: Dur(d.work),
            brk: Dur(d.brk),
            warn_before: Dur(d.warn_before),
            idle: Idle { credit: Dur(d.idle_credit), pause: Dur(d.idle_pause) },
            postpone: Postpone {
                duration: Dur(d.postpone),
                budget: d.postpone_budget,
                window: Dur(d.postpone_window),
            },
            calls: Calls { warn_after: Dur(d.defer_warn_after) },
            sound: crate::sound::Config::default(),
            animation: crate::overlay::Anim::default(),
        }
    }
}

impl Default for Idle {
    fn default() -> Self {
        FileConfig::default().idle
    }
}

impl Default for Postpone {
    fn default() -> Self {
        FileConfig::default().postpone
    }
}

impl Default for Calls {
    fn default() -> Self {
        FileConfig::default().calls
    }
}

impl From<FileConfig> for tea_core::Config {
    fn from(f: FileConfig) -> Self {
        Self {
            work: f.work.0,
            brk: f.brk.0,
            warn_before: f.warn_before.0,
            idle_credit: f.idle.credit.0,
            idle_pause: f.idle.pause.0,
            postpone: f.postpone.duration.0,
            postpone_budget: f.postpone.budget,
            postpone_window: f.postpone.window.0,
            defer_warn_after: f.calls.warn_after.0,
        }
    }
}

/// `$XDG_CONFIG_HOME/tea/config.toml`, falling back to `~/.config`.
pub fn default_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("tea").join("config.toml"))
}

pub fn load(path: &Path) -> Result<FileConfig, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    // toml's errors already carry line/column and a caret; don't bury that.
    toml::from_str(&text).map_err(|e| format!("in {}:\n{e}", path.display()))
}

/// Write the commented default file. Never overwrites.
pub fn write_default(path: &Path) -> Result<bool, String> {
    if path.exists() {
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    std::fs::write(path, DEFAULT_FILE)
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(true)
}

/// Reconcile settings that parse but can't work together.
///
/// Only genuinely unusable values are fatal. The rest are *derived* limits —
/// a postpone longer than the work interval, a warning that would fire before
/// you started — and those get clamped with a note. Refusing to start a break
/// timer over the size of a snooze button is the worst possible outcome for a
/// tool whose entire job is running in the background.
pub fn reconcile(c: &mut tea_core::Config) -> Result<Vec<String>, String> {
    if c.work.is_zero() || c.brk.is_zero() {
        return Err("work and break must both be greater than zero".into());
    }

    let mut notes = Vec::new();
    let mut clamp = |what: &str, value: &mut Duration, limit: Duration| {
        if *value >= limit {
            notes.push(format!("{what} {} capped to {}", human(*value), human(limit / 2)));
            *value = limit / 2;
        }
    };

    clamp("warn_before", &mut c.warn_before, c.work);
    if c.postpone_budget > 0 {
        clamp("postpone.duration", &mut c.postpone, c.work);
    }

    if c.idle_pause > c.idle_credit {
        notes.push(format!(
            "idle.pause {} capped to idle.credit {}",
            human(c.idle_pause),
            human(c.idle_credit)
        ));
        c.idle_pause = c.idle_credit;
    }

    Ok(notes)
}

/// A duration written as `"25m"`, or as a bare number meaning minutes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dur(pub Duration);

impl<'de> Deserialize<'de> for Dur {
    fn deserialize<D: de::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl de::Visitor<'_> for V {
            type Value = Dur;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str(r#"a duration like "90s", "25m" or "1h""#)
            }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<Dur, E> {
                parse(s)
                    .map(Dur)
                    .ok_or_else(|| E::custom(format!("{s:?} is not a duration like \"25m\"")))
            }
            // TOML hands every integer over as i64.
            fn visit_i64<E: de::Error>(self, n: i64) -> Result<Dur, E> {
                u64::try_from(n)
                    .ok()
                    .map(|n| Dur(Duration::from_secs(n * 60)))
                    .ok_or_else(|| E::custom(format!("{n} is not a number of minutes")))
            }
        }
        d.deserialize_any(V)
    }
}

pub fn parse(s: &str) -> Option<Duration> {
    let s = s.trim();
    // "ms" first: it also ends in 's'.
    let (num, scale) = if let Some(rest) = s.strip_suffix("ms") {
        (rest, 0.001)
    } else if let Some(rest) = s.strip_suffix('s') {
        (rest, 1.0)
    } else if let Some(rest) = s.strip_suffix('m') {
        (rest, 60.0)
    } else if let Some(rest) = s.strip_suffix('h') {
        (rest, 3600.0)
    } else {
        (s, 60.0) // bare numbers are minutes
    };

    // Fractions are allowed so animation timings can be written as "1.8s"
    // rather than in milliseconds.
    // Bounded before conversion: `Duration::from_secs_f64` panics on anything
    // too large, so an absurd value in a config file would take the daemon down
    // rather than report a bad setting.
    const MAX: f64 = 365.0 * 24.0 * 3600.0;
    let n: f64 = num.trim().parse().ok()?;
    let seconds = n * scale;
    (n.is_finite() && n >= 0.0 && seconds <= MAX).then(|| Duration::from_secs_f64(seconds))
}

/// A duration written so that `parse` can read it back. `human` is for people
/// and emits things like `1m30s`, which is not valid in the config file.
pub fn literal(d: Duration) -> String {
    // Sub-second values have no whole-unit spelling, so use milliseconds.
    if d.subsec_millis() != 0 {
        return format!("{}ms", d.as_millis());
    }
    let s = d.as_secs();
    if s == 0 {
        "0s".to_string()
    } else if s % 3600 == 0 {
        format!("{}h", s / 3600)
    } else if s % 60 == 0 {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
}

pub fn human(d: Duration) -> String {
    let s = d.as_secs();
    match (s / 3600, (s % 3600) / 60, s % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, 0) => format!("{m}m"),
        (0, m, s) => format!("{m}m{s:02}s"),
        (h, 0, _) => format!("{h}h"),
        (h, m, _) => format!("{h}h{m:02}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_default_file_parses_to_the_code_defaults() {
        let parsed: FileConfig = toml::from_str(DEFAULT_FILE).expect("default file must parse");
        let mut got: tea_core::Config = parsed.into();
        assert_eq!(got, tea_core::Config::default());
        assert!(reconcile(&mut got).unwrap().is_empty(), "defaults must need no clamping");
    }

    #[test]
    fn partial_file_keeps_defaults_for_everything_else() {
        let parsed: FileConfig = toml::from_str("work = \"50m\"\n").unwrap();
        let got: tea_core::Config = parsed.into();
        assert_eq!(got.work, Duration::from_secs(50 * 60));
        assert_eq!(got.brk, tea_core::Config::default().brk);
    }

    #[test]
    fn a_typo_is_an_error_not_a_silent_default() {
        let err = toml::from_str::<FileConfig>("wrok = \"50m\"\n").unwrap_err().to_string();
        assert!(err.contains("wrok"), "{err}");
    }

    #[test]
    fn fractions_and_milliseconds_parse() {
        assert_eq!(parse("1.8s"), Some(Duration::from_millis(1800)));
        assert_eq!(parse("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(parse("0s"), Some(Duration::ZERO));
        assert_eq!(parse("-1s"), None);
        // Would panic inside Duration if it were not rejected first.
        assert_eq!(parse("1e30s"), None);
        assert_eq!(parse("99999h"), None);
        assert_eq!(parse("inf"), None);
        assert_eq!(parse("25 minutes"), None);
    }

    #[test]
    fn durations_accept_units_and_bare_minutes() {
        let parsed: FileConfig =
            toml::from_str("work = 45\nbreak = \"90s\"\nwarn_before = \"1h\"\n").unwrap();
        assert_eq!(parsed.work.0, Duration::from_secs(45 * 60));
        assert_eq!(parsed.brk.0, Duration::from_secs(90));
        assert_eq!(parsed.warn_before.0, Duration::from_secs(3600));
    }

    #[test]
    fn bad_duration_names_the_offending_key() {
        let err = toml::from_str::<FileConfig>("work = \"25 minutes\"\n").unwrap_err().to_string();
        assert!(err.contains("work") && err.contains("25 minutes"), "{err}");
    }

    #[test]
    fn short_work_interval_clamps_instead_of_refusing_to_start() {
        // `tea --work 1` against the default 3m postpone: must still run.
        let mut c = tea_core::Config { work: Duration::from_secs(60), ..Default::default() };
        let notes = reconcile(&mut c).expect("a 1m work interval is legitimate");

        assert_eq!(c.postpone, Duration::from_secs(30));
        assert_eq!(c.warn_before, Duration::from_secs(30), "30s < 1m, left alone");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("postpone.duration"), "{notes:?}");
    }

    #[test]
    fn warn_before_is_clamped_not_fatal() {
        let mut c = tea_core::Config { work: Duration::from_secs(20), ..Default::default() };
        let notes = reconcile(&mut c).unwrap();
        assert_eq!(c.warn_before, Duration::from_secs(10));
        assert!(notes.iter().any(|n| n.contains("warn_before")), "{notes:?}");
    }

    #[test]
    fn disabled_postpone_is_never_clamped() {
        let mut c = tea_core::Config {
            work: Duration::from_secs(60),
            postpone_budget: 0,
            ..Default::default()
        };
        let notes = reconcile(&mut c).unwrap();
        assert_eq!(c.postpone, tea_core::Config::default().postpone);
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn idle_pause_is_clamped_to_credit() {
        let mut c = tea_core::Config::default();
        c.idle_pause = c.idle_credit + Duration::from_secs(1);
        let notes = reconcile(&mut c).unwrap();
        assert_eq!(c.idle_pause, c.idle_credit);
        assert!(notes.iter().any(|n| n.contains("idle.pause")), "{notes:?}");
    }

    #[test]
    fn every_literal_can_be_read_back() {
        // `set-work 90s` writes literal() into the file and parse() reads it on
        // the next start. If these two ever disagree, the command corrupts the
        // config it just edited.
        for secs in [1u64, 59, 60, 90, 1500, 3600, 5400, 7200, 86_400] {
            let d = Duration::from_secs(secs);
            assert_eq!(parse(&literal(d)), Some(d), "{secs}s did not survive the round trip");
        }
        for ms in [1u64, 250, 1800, 2500] {
            let d = Duration::from_millis(ms);
            assert_eq!(parse(&literal(d)), Some(d), "{ms}ms did not survive the round trip");
        }
    }

    #[test]
    fn zero_durations_are_still_fatal() {
        let mut c = tea_core::Config { brk: Duration::ZERO, ..Default::default() };
        assert!(reconcile(&mut c).is_err());
    }
}

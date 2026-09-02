//! What actually happened, one line at a time.
//!
//! Everything else tea persists is *now*: the state file holds one day of
//! numbers and `Tally::roll` wipes them at midnight, because a tally that never
//! rolls over is a number that only goes up and stops meaning anything by
//! Thursday. That is right for `tea status`, which is a question about the next
//! five minutes, and useless for the other question -- whether any of this is
//! working -- which is a question about the last three weeks.
//!
//! So: an append-only log, one JSON object per line. A line per break rather
//! than a row per day, because "how far did I walk on a break" is a question
//! about breaks, and a day's total cannot be taken apart again afterwards.
//!
//! Nothing here is load-bearing. A log that cannot be written must never cost
//! anybody a break, so every failure is complained about once and then ignored.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

/// Past this, the oldest half goes. At a break every twenty-five minutes this
/// is some years of walking, so it is a backstop against something going wrong
/// rather than a retention policy.
const CAP: u64 = 2 * 1024 * 1024;

/// How a break ended -- which is the only part of this that says whether the
/// tag on the wall is doing its job.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Gate {
    /// No gate: the countdown was the whole of it.
    #[default]
    None,
    /// The tag was scanned, and there was no walk to count.
    Scanned,
    /// The tag was scanned and the walk was made.
    Walked,
    /// Nothing arrived in time. The desk was handed back on `grace`.
    Grace,
}

/// One thing that happened, and when.
///
/// Internally tagged so a reader can switch on `kind` without knowing the
/// shape first -- the page does exactly that. `t` is repeated in each variant
/// rather than flattened in, because a flattened field is one more thing that
/// can go wrong in a file that has to survive being read by a future version.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Event {
    /// A break that ran and ended.
    Break {
        /// Unix seconds, at the end of it.
        t: u64,
        /// How long it ran for, in seconds -- the length the break actually
        /// had, which is not `break` on every fourth one.
        len: u64,
        /// Steps walked while the page was up, where a walk was counted at all.
        steps: u32,
        /// How many it wanted. Zero means steps were not part of this break.
        needed: u32,
        gate: Gate,
        /// Whether this was one of the long ones.
        long: bool,
    },
    /// A postpone granted: the interesting failure, and worth a line of its own.
    Postpone { t: u64 },
    /// A break paid for by being away from the keyboard rather than by a page.
    Credited {
        t: u64,
        /// How long you were away, in seconds.
        idle: u64,
    },
}

impl Event {
    /// When it happened, whatever it was.
    pub fn at(&self) -> u64 {
        match *self {
            Event::Break { t, .. } | Event::Postpone { t } | Event::Credited { t, .. } => t,
        }
    }
}

/// How a break that has just ended should be recorded.
///
/// A free function rather than a method on the engine so the four ways a break
/// can finish can be stated as four tests instead of four afternoons.
///
/// - `required` — whether the tag was standing between you and your desk at all.
/// - `gave_up` — the countdown ran out, nothing arrived, and `grace` expired.
/// - `released` — the gate was open when the page came down.
/// - `needed` — how many steps this break asked for; zero means none.
pub fn gate(required: bool, gave_up: bool, released: bool, needed: u32) -> Gate {
    match (required, gave_up, released) {
        // No gate to pass: the countdown was the whole of it.
        (false, _, _) => Gate::None,
        // Both are the same outcome from here -- the page came down without
        // the evidence it wanted -- and a break that ends unreleased ended on
        // the clock whether or not this tick was the one that said so.
        (_, true, _) | (_, _, false) => Gate::Grace,
        _ if needed > 0 => Gate::Walked,
        _ => Gate::Scanned,
    }
}

/// `$XDG_STATE_HOME/tea/history.jsonl`, beside the state file.
pub fn path() -> Option<PathBuf> {
    crate::state::dir().map(|d| d.join("history.jsonl"))
}

/// Said once and then never again, the same bargain the state file makes: a
/// message per break for the rest of the afternoon would be worse than the
/// missing line it is complaining about.
static COMPLAINED: AtomicBool = AtomicBool::new(false);

/// Add a line. Best effort by design -- see the module note.
pub fn append(event: &Event) {
    if let Err(e) = write(event)
        && !COMPLAINED.swap(true, Ordering::Relaxed)
    {
        eprintln!("tea: cannot record history ({e}); `tea dash` will be missing this");
    }
}

fn write(event: &Event) -> std::io::Result<()> {
    let path = path().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no state directory to write to")
    })?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }

    if std::fs::metadata(&path).is_ok_and(|m| m.len() > CAP) {
        trim(&path)?;
    }

    let mut line = serde_json::to_string(event)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push('\n');
    // One `write_all` of one short line to an O_APPEND handle: the kernel will
    // not interleave it with anybody else's, which is the whole reason this is
    // a log of lines rather than a document that has to be rewritten.
    let mut file = std::fs::OpenOptions::new().append(true).create(true).open(&path)?;
    file.write_all(line.as_bytes())
}

/// Drop the oldest half. Rewritten through a temporary file and renamed over,
/// so an interrupted trim leaves the whole log rather than half of one.
fn trim(path: &std::path::Path) -> std::io::Result<()> {
    let text = std::fs::read_to_string(path)?;
    let lines: Vec<&str> = text.lines().collect();
    let keep = lines.split_at(lines.len() / 2).1.join("\n");
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, format!("{keep}\n"))?;
    std::fs::rename(&tmp, path)
}

/// Everything recorded, oldest first.
///
/// A line that will not parse is skipped rather than fatal. A `kill -9` in the
/// middle of a write leaves half a line, and one truncated break must not be
/// the reason three weeks of them cannot be drawn.
pub fn read() -> Vec<Event> {
    let Some(path) = path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    text.lines().filter_map(|line| serde_json::from_str(line).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_break() -> Event {
        Event::Break { t: 1_756_819_511, len: 300, steps: 47, needed: 20, gate: Gate::Walked, long: false }
    }

    #[test]
    fn a_break_survives_the_round_trip() {
        let line = serde_json::to_string(&a_break()).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&line).unwrap(), a_break());
    }

    #[test]
    fn the_kind_is_in_the_line_where_a_reader_can_switch_on_it() {
        let line = serde_json::to_string(&Event::Postpone { t: 7 }).unwrap();
        assert_eq!(line, r#"{"kind":"postpone","t":7}"#);
    }

    #[test]
    fn a_half_written_line_costs_that_line_and_nothing_else() {
        let text = format!(
            "{}\n{{\"kind\":\"brea\n{}\n",
            serde_json::to_string(&a_break()).unwrap(),
            serde_json::to_string(&Event::Postpone { t: 9 }).unwrap(),
        );
        let kept: Vec<Event> =
            text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        assert_eq!(kept, vec![a_break(), Event::Postpone { t: 9 }], "the good lines still draw");
    }

    #[test]
    fn a_kind_from_a_later_version_is_skipped_rather_than_fatal() {
        let line = r#"{"kind":"whatever-comes-next","t":1}"#;
        assert!(serde_json::from_str::<Event>(line).is_err());
    }

    #[test]
    fn trimming_keeps_the_newest_half() {
        let dir = std::env::temp_dir().join(format!("tea-history-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.jsonl");
        let text: String =
            (0..10).map(|n| format!("{}\n", serde_json::to_string(&Event::Postpone { t: n }).unwrap())).collect();
        std::fs::write(&path, text).unwrap();

        trim(&path).unwrap();

        let kept = std::fs::read_to_string(&path).unwrap();
        let times: Vec<u64> = kept
            .lines()
            .filter_map(|l| serde_json::from_str::<Event>(l).ok())
            .map(|e| e.at())
            .collect();
        assert_eq!(times, (5..10).collect::<Vec<_>>(), "the recent walking is the part worth keeping");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_break_with_no_tag_in_front_of_it_is_recorded_as_ungated() {
        assert_eq!(gate(false, false, true, 20), Gate::None, "the tag is off; nothing was asked");
        assert_eq!(gate(false, true, false, 0), Gate::None);
    }

    #[test]
    fn a_scan_and_a_walk_are_told_apart() {
        assert_eq!(gate(true, false, true, 20), Gate::Walked, "steps were part of this gate");
        assert_eq!(gate(true, false, true, 0), Gate::Scanned, "the tag alone was the gate");
    }

    #[test]
    fn a_break_that_ended_without_its_evidence_says_so() {
        assert_eq!(gate(true, true, false, 20), Gate::Grace, "grace ran out");
        assert_eq!(
            gate(true, false, false, 20),
            Gate::Grace,
            "unreleased at the end is the same outcome, whichever tick noticed",
        );
    }

    #[test]
    fn every_event_can_say_when_it_was() {
        assert_eq!(a_break().at(), 1_756_819_511);
        assert_eq!(Event::Credited { t: 4, idle: 400 }.at(), 4);
    }
}

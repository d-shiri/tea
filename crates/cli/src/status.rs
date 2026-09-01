//! `tea status` — what the background service is doing right now.
//!
//! There is no IPC, so this reads the state file the service already writes
//! (every 5s, and on every change worth keeping) and asks the session bus for
//! the live bits. Accurate to within one save interval, which is close enough
//! to read off a terminal.

use crate::clock;
use crate::config::{Hours, human};
use crate::session::Session;
use crate::state;
use tea_core::Config;
use std::io::IsTerminal;
use std::time::Duration;

/// Beyond this, the file is not being refreshed and nothing is running.
const FRESH: Duration = Duration::from_secs(15);
/// Total width that `tea status` and `tea config` both line up to.
pub const WIDTH: usize = 68;
const BAR: usize = 24;

pub fn show(cfg: Config, hours: &Hours, config_path: &std::path::Path, boottime: Duration) {
    let s = Style::new();
    let store = state::Store::new();
    let saved = store.as_ref().and_then(|st| st.load(boottime));

    println!();
    let Some(saved) = saved else {
        heading(&s, "tea — never run", &tilde(config_path));
        println!("  {}", s.dim("nothing saved yet; start it and check back"));
        println!();
        return;
    };

    let running = saved.gap <= FRESH;
    let snap = saved.snapshot;
    // The break on screen is not always `cfg.brk`: every so often it is the
    // long one, and a gauge that measured it against the short one would show
    // a break three hundred percent through.
    let brk = match snap.breaking && !snap.break_len.is_zero() {
        true => snap.break_len,
        false => cfg.brk,
    };
    // Switched off by hand, or out of hours: either way nothing is counting,
    // and a work gauge creeping along beside it would be a lie.
    let off = state::off::left();
    let asleep = !hours.awake(clock::now());
    let mut today = saved.tally.clone();
    today.roll(&clock::today());

    // Project forward over the seconds since the last save. Within one save
    // interval this is exact enough; when nothing is running it would be a lie,
    // so it is only done while fresh.
    let elapsed = if running { saved.gap } else { Duration::ZERO };

    let mut session = Session::connect();
    let held_by = session.inhibitors();

    // ---- headline ----------------------------------------------------------
    let state = if !running {
        "tea — not running".to_string()
    } else if let Some(left) = off {
        s.yellow(&format!("tea — off for another {}", human(left)))
    } else if asleep {
        s.yellow(&format!("tea — asleep, {}", hours.opens()))
    } else if snap.breaking && cfg.require_release && !snap.released
        && (snap.rested + elapsed) >= cfg.brk
    {
        s.yellow("tea — waiting for the tag")
    } else if snap.breaking {
        s.green("tea — on a break")
    } else if snap.due {
        s.yellow("tea — break due, waiting")
    } else {
        "tea — working".to_string()
    };
    heading(&s, &state, &tilde(config_path));
    println!();

    // ---- the main gauge ----------------------------------------------------
    if off.is_some() || asleep {
        // Neither gauge means anything while the clock is stopped, and a work
        // bar that has not moved since Friday is worse than no bar at all.
        let why = match off {
            Some(_) => "switched off by hand — `tea on` starts it again".to_string(),
            None => format!("outside {} — nothing is being counted", hours.describe()),
        };
        println!("  {}  {}", s.dim("paused  "), s.dim(&why));
    } else if snap.breaking {
        let rested = (snap.rested + elapsed).min(brk);
        let left = brk.saturating_sub(rested);
        println!("  {}  {}", s.dim("rest    "), bar(rested, brk, &s));
        // Time served and still up means it is waiting on the tag, and "back to
        // work in 0s" beside a page that is plainly still there reads as a bug.
        let waiting = cfg.require_release && left.is_zero() && !snap.released;
        println!(
            "            {} of {}{} — {}",
            human(rested),
            human(brk),
            match brk != cfg.brk {
                true => " (the long one)",
                false => "",
            },
            if waiting {
                s.bold("waiting for the tag")
            } else {
                format!("back to work in {}", s.bold(&human(left)))
            }
        );
    } else {
        let worked = (snap.worked + elapsed).min(cfg.work);
        let left = cfg.work.saturating_sub(worked);
        println!("  {}  {}", s.dim("work    "), bar(worked, cfg.work, &s));
        let when = if snap.due {
            match held_by.as_slice() {
                [] => "now — waiting on something".to_string(),
                names => format!("held by {}", names.join(", ")),
            }
        } else {
            format!("next break in {}", human(left))
        };
        println!("            {} of {} — {}", human(worked), human(cfg.work), s.bold(&when));
    }
    println!();

    // ---- the rest ----------------------------------------------------------
    let left = cfg.postpone_budget.saturating_sub(snap.postpones_used);
    let postpone = if cfg.postpone_budget == 0 {
        "disabled".to_string()
    } else {
        format!(
            "{left} of {} left — resets in {}",
            cfg.postpone_budget,
            human(cfg.postpone_window.saturating_sub(snap.window_elapsed))
        )
    };
    row(&s, "postpone", &postpone);

    // The day, which is the only row here that says whether any of this is
    // working. Everything else is about the next five minutes.
    let day = match today.quiet() {
        true => "nothing yet today".to_string(),
        false => {
            let mut parts = vec![match today.breaks {
                1 => "1 break".to_string(),
                n => format!("{n} breaks"),
            }];
            if today.credited > 0 {
                parts.push(format!("{} away from the desk", today.credited));
            }
            if today.postponed > 0 {
                parts.push(format!("{} postponed", today.postponed));
            }
            if today.steps > 0 {
                parts.push(format!("{} steps walked", today.steps));
            }
            parts.join(" · ")
        }
    };
    row(&s, "today", &day);

    if let Some(until) = long_break(&cfg, snap.breaks_done) {
        row(&s, "long", &until);
    }

    let idle = match session.idle() {
        Some(d) => format!("idle {}", human(d)),
        None => "idle unknown".to_string(),
    };
    let holding = match held_by.as_slice() {
        [] => "nothing is holding a break".to_string(),
        names => format!("held by {}", names.join(", ")),
    };
    row(&s, "now", &format!("{idle} · {holding}"));

    row(
        &s,
        "service",
        &if running {
            format!("running · saved {} ago", human(saved.gap))
        } else {
            s.yellow(&format!("stopped · last saved {} ago", human(saved.gap)))
        },
    );
    println!();
}

/// When the long break falls due, if there is one. Worked out here rather than
/// asked of the scheduler because `tea status` has a state file and no
/// scheduler -- there is no daemon in this process to ask.
fn long_break(cfg: &Config, breaks_done: u32) -> Option<String> {
    if cfg.long_every == 0 || cfg.long_brk.is_zero() {
        return None;
    }
    let until = (cfg.long_every - 1) - (breaks_done % cfg.long_every);
    Some(match until {
        0 => format!("{} — the next break is the long one", human(cfg.long_brk)),
        1 => format!("{} — after one more ordinary break", human(cfg.long_brk)),
        n => format!("{} — after {n} more ordinary breaks", human(cfg.long_brk)),
    })
}

/// A bold title with something dim pushed out to the right margin.
pub fn heading(s: &Style, title: &str, right: &str) {
    // Styling adds invisible characters, so pad from the visible length.
    let visible = strip(title).chars().count() + right.chars().count();
    println!("  {}{}{}", s.bold(title), " ".repeat(WIDTH.saturating_sub(visible)), s.dim(right));
}

/// Visible length of a string that may already carry colour codes.
fn strip(text: &str) -> String {
    let mut out = String::new();
    let mut in_escape = false;
    for c in text.chars() {
        match (in_escape, c) {
            (false, '\u{1b}') => in_escape = true,
            (true, 'm') => in_escape = false,
            (true, _) => {}
            (false, c) => out.push(c),
        }
    }
    out
}

/// `/home/you/.config/...` is mostly noise; `~/.config/...` is not.
pub fn tilde(p: &std::path::Path) -> String {
    let text = p.display().to_string();
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => {
            let home = home.to_string_lossy().into_owned();
            text.strip_prefix(&home).map(|rest| format!("~{rest}")).unwrap_or(text)
        }
        _ => text,
    }
}

fn row(s: &Style, label: &str, value: &str) {
    println!("  {}  {value}", s.dim(&format!("{label:<8}")));
}

fn bar(done: Duration, total: Duration, s: &Style) -> String {
    let fraction = if total.is_zero() {
        0.0
    } else {
        (done.as_secs_f64() / total.as_secs_f64()).clamp(0.0, 1.0)
    };
    let filled = (fraction * BAR as f64).round() as usize;
    format!(
        "{}{}  {:>3.0}%",
        s.green(&"█".repeat(filled)),
        s.dim(&"░".repeat(BAR - filled)),
        fraction * 100.0
    )
}

/// Colour only when a person is looking; piping into a file or `grep` should
/// not come out full of escape codes.
pub struct Style {
    on: bool,
}

impl Style {
    pub fn new() -> Self {
        Self { on: std::io::stdout().is_terminal() }
    }
    fn wrap(&self, code: &str, text: &str) -> String {
        if self.on { format!("\x1b[{code}m{text}\x1b[0m") } else { text.to_string() }
    }
    pub fn bold(&self, t: &str) -> String {
        self.wrap("1", t)
    }
    pub fn dim(&self, t: &str) -> String {
        self.wrap("2", t)
    }
    pub fn green(&self, t: &str) -> String {
        self.wrap("32", t)
    }
    pub fn yellow(&self, t: &str) -> String {
        self.wrap("33", t)
    }
}

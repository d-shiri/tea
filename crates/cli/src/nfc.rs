//! The tag on the wall in the other room.
//!
//! Sitting out a break at your own desk is not a break. The fix is physical:
//! an NFC tag somewhere you have to stand up and walk to, and a break page that
//! does not lift until the tag says you went. This module is the ear — a very
//! small HTTP server whose entire vocabulary is "the tag was scanned".
//!
//! It rides the GTK main loop like everything else here: `gio`'s socket
//! service accepts and reads asynchronously, so there are still no threads and
//! nothing to lock. And it never touches the scheduler. A request can land in
//! the middle of a tick, so the two speak through [`Link`] instead — the
//! engine leaves its state there once a second, the server leaves its scan
//! there whenever one arrives, exactly the way the postpone button works.

use crate::clock;
use crate::config::{self, human};
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use serde::{Deserialize, de};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

/// A scan is one small packet; the settings page sends the whole config file
/// back. Anything past this is neither.
const REQUEST_CAP: usize = 64 * 1024;
/// A connection that has not finished asking by now never will.
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the list gets to answer before the poll gives up on it for this
/// beat. Much shorter than the gate's own timeout on purpose: nothing waits on
/// this, and a slow hub must cost the page a stale panel rather than a late
/// step count.
const JOBS_TIMEOUT: u32 = 2;
/// How often the list is actually asked for, whatever the poll interval is.
/// Household jobs do not change between one second and the next, and this is a
/// service call rather than a state lookup.
const JOBS_EVERY: Duration = Duration::from_secs(10);
/// How long a break waits before the first poke, and how often they come after
/// that.
///
/// Ten seconds before the first: sooner is asking a phone about a walk that
/// has not started, because the first ten seconds of a break are spent
/// standing up. Thirty between the rest. A phone in a pocket with the screen
/// off holds most pokes back anyway, and the step count it reports comes from
/// Health Connect, which is written in batches a minute or two apart -- so a
/// poke every ten seconds was a phone spending the break being notified about
/// a number that had not changed since the last one. Thirty is often enough
/// that a batch is picked up within half a minute of landing, and seldom
/// enough that nobody's pocket buzzes for nothing.
const NUDGE_FIRST: Duration = Duration::from_secs(10);
const NUDGE_EVERY: Duration = Duration::from_secs(30);
/// What the phone gets, and the only message tea ever sends one. The Android
/// companion app reads it as an order to report every sensor it has, now.
const NUDGE_BODY: &str = "{\"message\":\"command_update_sensors\"}";
/// Like the list's: nothing waits on the poke, so a slow hub must not be what
/// the gate's own questions queue behind.
const NUDGE_TIMEOUT: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// No port, no gate: breaks end when the countdown does.
    // Off by default and it must stay that way. Switching this on opens a
    // socket and changes when a break ends -- neither is something to inherit
    // from an upgrade you did not read the notes for.
    #[default]
    #[serde(alias = "disabled", alias = "inactive", alias = "false")]
    Off,
    /// Listen for the tag, and hold the page until it is scanned.
    #[serde(alias = "enabled", alias = "active", alias = "true")]
    On,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub mode: Mode,
    /// `address:port`. Filled in from `[port]` when the file is loaded; a
    /// file from before `[port]` existed still writes it here, and is read.
    pub listen: String,
    /// Shared secret, likewise from `[port]`. Anyone who can reach the port
    /// and knows this can end your break, so the ear stays shut without it.
    pub token: String,
    /// Give up on the tag after this long and hand the desk back anyway.
    /// `"off"` waits for as long as it takes.
    pub grace: Grace,
    /// What the page says while it waits. Yours knows where your tag is.
    pub prompt: String,
    /// Ask Home Assistant about the tag instead of waiting to be told.
    pub home_assistant: HomeAssistant,
    /// The other half of the gate: steps walked while the page is up.
    pub steps: Steps,
    /// And a third: the phone's own word that its owner is on the move.
    pub moving: Moving,
    /// Not part of the gate at all: what to *do* with the break, read off a
    /// to-do list on the same hub.
    pub chores: Chores,
    /// Where the tag's URL actually points, when tea is not reached directly.
    ///
    /// Nothing has to listen on your network for a tag to work: a reverse proxy
    /// on a machine that is already listening, with the laptop dialling *out*
    /// to it, gets the scan here without a single inbound port. tea cannot know
    /// that address, so it is told. Empty means the tag talks to `listen`.
    pub url: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::Off,
            listen: String::new(),
            token: String::new(),
            grace: Grace(Duration::from_secs(10 * 60)),
            prompt: "Scan the tag to get your desk back".into(),
            url: String::new(),
            home_assistant: HomeAssistant::default(),
            steps: Steps::default(),
            moving: Moving::default(),
            chores: Chores::default(),
        }
    }
}

impl Config {
    pub fn on(&self) -> bool {
        self.mode == Mode::On
    }

    /// What to write onto the tag.
    ///
    /// `url` wins when it is set, because with anything in front of tea the
    /// address it listens on is not an address the tag can reach. Forgiving
    /// about the path: both `https://tea.example` and `https://tea.example/unlock`
    /// are what someone means.
    pub fn tag_url(&self) -> String {
        let front = self.url.trim().trim_end_matches('/');
        let base = match front {
            "" => format!("http://{}/unlock", self.listen),
            url if url.ends_with("/unlock") => url.to_string(),
            url => format!("{url}/unlock"),
        };
        format!("{base}?token={}", self.token)
    }

    /// Whether the scan arrives by tea asking for it, rather than being told.
    pub fn asks(&self) -> bool {
        self.home_assistant.on()
    }

    /// Whether something else is the front door.
    pub fn fronted(&self) -> bool {
        !self.url.trim().is_empty()
    }

    /// Whether this break also has to be walked off.
    ///
    /// Steps are read from the same hub as the tag and nowhere else, so asking
    /// is a precondition: with no hub there is no step count, and a gate whose
    /// second half can never be satisfied is a locked screen.
    pub fn counts_steps(&self) -> bool {
        self.on() && self.asks() && self.steps.on()
    }

    /// Whether the phone's word that you are moving is part of the gate. Read
    /// from the hub like the steps, and needs it for the same reason.
    pub fn counts_moving(&self) -> bool {
        self.on() && self.asks() && self.moving.on()
    }

    /// Whether the page shows a list of jobs to do with the break.
    ///
    /// Read off the same hub as everything else here, so asking is a
    /// precondition -- but *not* gated on the tag: a list is something the page
    /// shows, not something that holds the desk, and it is perfectly reasonable
    /// to want the jobs without wanting the walk.
    pub fn shows_chores(&self) -> bool {
        self.asks() && self.chores.on()
    }

    /// Why the list is on in the file and off in the process, if it is.
    pub fn chores_misconfigured(&self) -> Option<String> {
        if self.chores.mode != Mode::On {
            return None;
        }
        if !self.asks() {
            return Some(
                "nfc.chores is on, but no hub is being watched — the list comes from Home \
                 Assistant (nfc.home_assistant.url and entity)"
                    .into(),
            );
        }
        if self.chores.entity.trim().is_empty() {
            return Some("nfc.chores is on, but nfc.chores.entity is empty — no list to read".into());
        }
        if !self.chores.entity.trim().starts_with("todo.") {
            return Some(format!(
                "nfc.chores.entity {:?} is not a to-do list — it wants a `todo.` entity",
                self.chores.entity.trim()
            ));
        }
        if self.chores.show == 0 {
            return Some("nfc.chores.show is 0, so the page has no room for the list".into());
        }
        None
    }

    /// Seconds of moving this break asks for, scaled from the steps when the
    /// steps are counted at all.
    pub fn moving_secs(&self) -> u32 {
        self.moving.secs(self.counts_steps().then_some(self.steps.count))
    }

    /// The same, with where the number came from.
    pub fn moving_words(&self) -> String {
        self.moving.describe(self.counts_steps().then_some(self.steps.count))
    }

    /// Why moving is on in the file and off in the process, if it is.
    pub fn moving_misconfigured(&self) -> Option<String> {
        if self.moving.mode != Mode::On || !self.on() {
            return None;
        }
        if !self.asks() {
            return Some(
                "nfc.moving is on, but no hub is being watched — the activity comes from Home \
                 Assistant (nfc.home_assistant.url and entity)"
                    .into(),
            );
        }
        if self.moving.entity.trim().is_empty() {
            return Some("nfc.moving is on, but nfc.moving.entity is empty — nothing to watch".into());
        }
        if matches!(self.moving.r#for, Wanted::For(d) if d.is_zero()) {
            return Some("nfc.moving.for is 0, so no time on your feet is being asked for".into());
        }
        if self.moving.states.iter().all(|s| s.trim().is_empty()) {
            return Some("nfc.moving.states is empty — no state would count as moving".into());
        }
        None
    }

    /// Set and switched on, but with nothing to read it from. Worth saying out
    /// loud at startup: the alternative is a setting that looks on in the file
    /// and silently is not.
    pub fn steps_misconfigured(&self) -> Option<String> {
        if self.steps.mode != Mode::On || !self.on() {
            return None;
        }
        if !self.asks() {
            return Some(
                "nfc.steps is on, but no hub is being watched — steps come from Home \
                 Assistant, so nfc.home_assistant needs a url and an entity"
                    .into(),
            );
        }
        if self.steps.entity.trim().is_empty() {
            return Some("nfc.steps is on, but nfc.steps.entity is empty — nothing to count".into());
        }
        if self.steps.count == 0 {
            return Some("nfc.steps.count is 0, so no walk is being asked for".into());
        }
        None
    }

    /// Why the phone is not being poked, when the file names one to poke.
    pub fn nudge_misconfigured(&self) -> Option<String> {
        let ha = &self.home_assistant;
        let (domain, service) = ha.nudge_call()?;
        if ha.url.trim().is_empty() {
            return Some(
                "nfc.home_assistant.nudge names a phone, but url is empty — the poke goes \
                 through the hub"
                    .into(),
            );
        }
        if !well_formed(domain, service) {
            return Some(format!(
                "nfc.home_assistant.nudge {:?} is not a service like \"notify.mobile_app_pixel\"",
                ha.nudge.trim()
            ));
        }
        if !self.counts_steps() && !self.counts_moving() {
            return Some(
                "nfc.home_assistant.nudge names a phone, but nothing is waiting on one — \
                 nfc.steps and nfc.moving are both off"
                    .into(),
            );
        }
        None
    }
}

/// How far you have to go before the page lifts.
///
/// A tag on the wall proves you stood up; it does not prove you went anywhere,
/// and a tag within reach of the chair proves nothing at all. The step count
/// is the part that cannot be leaned over to reach. Both halves have to be in
/// -- the scan and the walk -- and neither one alone ends the break.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Steps {
    /// Off by default, for the same reason the tag is: an upgrade must never
    /// quietly add a second thing standing between you and your desk.
    pub mode: Mode,
    /// How many steps the break wants. Counted from where you were when the
    /// page went up, so a daily total is a perfectly good sensor to point at.
    pub count: u32,
    /// The sensor holding the count -- a phone's daily step total, a watch, a
    /// Health Connect feed. Any entity whose state is a rising number will do.
    pub entity: String,
    /// How promptly the sensor reports what has been walked. Decides whether
    /// the first rise seen mid-break is a walk or the phone catching up.
    pub sync: Sync,
}

impl Default for Steps {
    fn default() -> Self {
        // Twenty steps is a walk out of the room and back to the doorway. Small
        // enough that nobody games it by shuffling, large enough that it cannot
        // be done from the chair.
        Self { mode: Mode::Off, count: 20, entity: String::new(), sync: Sync::Batched }
    }
}

/// When the sensor says what has been walked: as it happens, or whenever the
/// phone gets round to it.
///
/// The difference decides what the first rise of a break means. From a feed
/// that lags -- Health Connect, a watch that uploads when it feels like it --
/// the reading a break starts from is minutes or hours old, and the first rise
/// after it holds everything walked since, most of it from before the page went
/// up. From the phone's own step counter, reported every minute or so, that
/// same rise is the walk itself, and refusing it sends somebody who has just
/// crossed the flat back across it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sync {
    /// The total arrives in batches, minutes or hours after the steps. The
    /// first rise of a break moves the mark; only what comes after it counts.
    /// The default, because it is the one that can never open the gate from
    /// the chair.
    #[default]
    #[serde(alias = "lagging", alias = "delayed", alias = "slow")]
    Batched,
    /// The total keeps up with your feet, give or take a minute. Every rise
    /// counts, the first one included.
    #[serde(alias = "fast", alias = "realtime", alias = "prompt")]
    Live,
}

impl Steps {
    pub fn on(&self) -> bool {
        self.mode == Mode::On && self.count > 0 && !self.entity.trim().is_empty()
    }
}

/// How the walk is going: steps counted since this break began, and how many
/// the gate is holding out for. A `needed` of zero means steps are not part of
/// this break at all, which is what every ungated break looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Walk {
    pub walked: u32,
    pub needed: u32,
    /// The count has been re-based since the page went up: the first thing the
    /// phone reported mid-break was steps from before it, so it moved the mark
    /// instead of paying for the break.
    ///
    /// Carried to the page rather than left in the log, because until it is
    /// said out loud the page reads *0 of 50* at somebody who has just walked
    /// across the flat, and they walk it again.
    pub marked: bool,
}

impl Walk {
    pub fn done(&self) -> bool {
        self.walked >= self.needed
    }

    pub fn left(&self) -> u32 {
        self.needed.saturating_sub(self.walked)
    }
}

/// One line of the list: what it says, and whether it has been ticked off.
///
/// The `uid` never reaches the page. It is how a job keeps its identity between
/// one poll and the next -- summaries are not unique, and "Clean windows" done
/// is the same row as "Clean windows" not done, not a new one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Job {
    pub uid: String,
    pub summary: String,
    pub done: bool,
    /// When it was ticked off, as the hub wrote it -- an ISO 8601 instant, in
    /// UTC, which is why it can be sorted as a string. Empty on anything still
    /// to do. Never shown: it decides *which* finished jobs are worth one of
    /// the few lines the corner has, and nothing else.
    pub completed: String,
    /// When it is wanted by, as the hub wrote it: a date, or a date and time
    /// with its zone. Empty for most jobs.
    pub due: String,
}

/// The list as the page shows it: which list, and the handful of lines from it
/// that fit in the corner.
///
/// Laid out afresh on every answer: what is still to do at the top, in the
/// list's own order, and what has been ticked off underneath it, freshest
/// first. Tick a job off from the phone and it drops below the open ones
/// rather than sitting struck through in the middle of them -- the top of
/// the corner is always the next thing to do, and the bottom is always what
/// has been done.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Board {
    /// What the page draws above the jobs.
    pub title: String,
    pub jobs: Vec<Job>,
    /// Open jobs the page has no room for. The panel says so outright -- a
    /// corner that showed four of six and let a number elsewhere claim six
    /// is a corner whose arithmetic does not check out, and the reader is the
    /// one left doing the subtraction.
    pub hidden: u32,
    /// How many jobs on the list were ticked off today, by the hub's own
    /// stamps and the local calendar. The whole list, not just the rows shown.
    ///
    /// Read off the list rather than counted by whoever has the page up, so
    /// it is the same number on a real break, on `tea run`, and after a
    /// restart -- and a job done at the kitchen table between breaks is still
    /// a job done today. What a *break* is credited with is a different
    /// question, and stays with the tally.
    pub today: u32,
}

impl Board {
    /// What the page is told: the words and the ticks, without the identities
    /// that only the poll has any use for.
    pub fn chores(&self) -> Vec<tea_core::Chore> {
        self.jobs
            .iter()
            .map(|job| tea_core::Chore {
                summary: job.summary.clone(),
                done: job.done,
                due: (!job.done)
                    .then(|| clock::deadline(&job.due))
                    .flatten()
                    .map(|d| tea_core::Due { label: d.label, late: d.late, soon: d.soon }),
            })
            .collect()
    }
}

/// The third half of the gate: the phone's own word that its owner is on the
/// move, from Android's activity recognition rather than from a step count.
///
/// Steps can be earned by a phone waved at the desk. Activity recognition
/// wants the whole body going somewhere, and says so in a word: `walking`,
/// `on_foot`, `running`. This asks for a little time in one of those states,
/// added up over the break, and is the cheapest honest check there is -- no
/// hardware, one sensor switched on in the companion app.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Moving {
    /// Off by default, like the other two: an upgrade must never quietly add
    /// a third thing standing between you and your desk.
    pub mode: Mode,
    /// The entity, `sensor.<phone>_detected_activity` from the companion app.
    pub entity: String,
    /// How long in a moving state the break wants, in total. `"auto"` works
    /// it out from the steps asked for; a duration says it outright. Android
    /// reports lazily, a minute behind at times, so this is a floor and not a
    /// stopwatch.
    #[serde(rename = "for")]
    pub r#for: Wanted,
    /// Which of the sensor's words count as moving.
    pub states: Vec<String>,
}

impl Default for Moving {
    fn default() -> Self {
        Self {
            mode: Mode::Off,
            entity: String::new(),
            r#for: Wanted::Auto,
            states: ["walking", "on_foot", "running"].map(String::from).to_vec(),
        }
    }
}

impl Moving {
    pub fn on(&self) -> bool {
        self.mode == Mode::On
            && !self.entity.trim().is_empty()
            && !matches!(self.r#for, Wanted::For(d) if d.is_zero())
            && self.states.iter().any(|s| !s.trim().is_empty())
    }

    /// Whole seconds asked for, given how many steps the break asks for --
    /// `None` when it asks for none.
    ///
    /// On auto, a share of the time the walk itself takes: a hundred steps
    /// is a minute of walking and asks for thirty seconds of the phone
    /// saying so, ten steps asks for five. Half rather than all of it because
    /// the sensor is late and lumpy, and a floor the walk cannot reach opens
    /// the gate on grace instead of on the walk. Without a walk to scale
    /// from, thirty seconds: out of the room, and not only to the doorway.
    pub fn secs(&self, steps: Option<u32>) -> u32 {
        match self.r#for {
            Wanted::For(d) => d.as_secs().clamp(1, 3600) as u32,
            Wanted::Auto => match steps {
                Some(count) => ((f64::from(count) * SECS_PER_STEP).round() as u32).clamp(5, 120),
                None => 30,
            },
        }
    }

    /// How it reads in `tea config`: the number, and where it came from.
    pub fn describe(&self, steps: Option<u32>) -> String {
        match (self.r#for, steps) {
            (Wanted::For(_), _) => format!("{}s", self.secs(steps)),
            (Wanted::Auto, Some(count)) => format!("{}s — auto, from {count} steps", self.secs(steps)),
            (Wanted::Auto, None) => format!("{}s — auto, with no walk to scale from", self.secs(steps)),
        }
    }

    /// Whether what the sensor said counts as moving.
    pub fn counts(&self, state: &str) -> bool {
        let state = state.trim();
        self.states.iter().any(|s| s.trim().eq_ignore_ascii_case(state))
    }
}

/// What to do with the five minutes, read off a list you keep elsewhere.
///
/// The rest of this module is about *making* you get up. This is the only part
/// that is about what to do once you have: a break page that says "stand up"
/// and nothing else leaves you standing in the kitchen wondering why. A
/// Home Assistant to-do list is already where the household jobs live, already
/// editable from the phone in your hand, and already synced -- so the page
/// borrows it rather than inventing a list of its own.
///
/// Read-only, deliberately. Ticking a job off from a laptop you have been sent
/// away from is a lie the page would have to take at face value, and the list
/// is one tap away on the phone you are holding while you do the job.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Chores {
    /// Off by default, like everything else that talks to the hub.
    pub mode: Mode,
    /// The list, `todo.<name>`. Any `todo` entity will do.
    pub entity: String,
    /// What to call it on the page. Empty means the entity id, which is what
    /// the list is actually called and nobody's idea of a heading.
    pub title: String,
    /// How many jobs the page has room for. The list itself can be as long as
    /// you like; this is what fits in the corner of a break page without
    /// becoming a second thing to read. Whatever does not fit is counted on a
    /// line of its own, so the rows never quietly disagree with a total.
    pub show: u32,
}

impl Default for Chores {
    fn default() -> Self {
        Self { mode: Mode::Off, entity: String::new(), title: String::new(), show: 8 }
    }
}

impl Chores {
    pub fn on(&self) -> bool {
        self.mode == Mode::On && !self.entity.trim().is_empty() && self.show > 0
    }

    /// What the page puts above the jobs.
    pub fn header(&self) -> String {
        match self.title.trim() {
            "" => self.entity.trim().to_string(),
            title => title.to_string(),
        }
    }

    /// The list, capped: never more than the page can hold, and never so many
    /// that a hub with a hundred jobs on it hands back a page of them.
    pub fn cap(&self) -> usize {
        self.show.clamp(1, CHORES_CAP) as usize
    }
}

/// The most lines the corner will ever show, whatever the config says. Past
/// this the panel stops being a glance and starts being homework.
const CHORES_CAP: u32 = 10;

/// How many of the page's lines are held back for jobs already done.
///
/// Without this a long enough list of things still to do fills every line, and
/// a panel that can only ever show work outstanding is a nag. Two struck-through
/// lines at the bottom are what makes it a record of a day going well -- and
/// they are the freshest two, because a job ticked off this morning is worth
/// seeing and one from last Tuesday is not.
const CHORES_DONE_SLOTS: usize = 2;

/// Seconds of moving asked for per step asked for, on auto. A hundred steps
/// at walking pace is about a minute; half of that is what the phone has to
/// have noticed.
const SECS_PER_STEP: f64 = 0.3;

/// How long the moving half wants: worked out from the steps, or said.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Wanted {
    /// Scaled from `nfc.steps.count` -- see `Moving::secs`.
    #[default]
    Auto,
    For(Duration),
}

impl<'de> Deserialize<'de> for Wanted {
    fn deserialize<D: de::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl de::Visitor<'_> for V {
            type Value = Wanted;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str(r#""auto", or a duration like "30s""#)
            }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<Wanted, E> {
                if s.trim().eq_ignore_ascii_case("auto") {
                    return Ok(Wanted::Auto);
                }
                config::parse(s).map(Wanted::For).ok_or_else(|| {
                    E::custom(format!("{s:?} is not \"auto\" or a duration like \"30s\""))
                })
            }
            fn visit_i64<E: de::Error>(self, n: i64) -> Result<Wanted, E> {
                // A bare number is seconds here, not minutes: nobody wants
                // three minutes of walking, and `for = 30` reads as thirty.
                u64::try_from(n)
                    .ok()
                    .map(|secs| Wanted::For(Duration::from_secs(secs)))
                    .ok_or_else(|| E::custom(format!("{n} is not a number of seconds")))
            }
        }
        d.deserialize_any(V)
    }
}

/// How the time on your feet is going: seconds counted in a moving state since
/// this break began, and how many the gate is holding out for. A `needed` of
/// zero means moving is not part of this break at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Motion {
    pub secs: u32,
    pub needed: u32,
    /// The sensor cannot be read, so this half will never close on its own.
    /// Carried to the page so it can say so, rather than showing a count that
    /// is never going to move.
    pub lost: bool,
}

impl Motion {
    pub fn done(&self) -> bool {
        self.secs >= self.needed
    }

    pub fn left(&self) -> u32 {
        self.needed.saturating_sub(self.secs)
    }
}

/// A duration that is allowed to be `"off"`.
///
/// Zero and `"off"` mean the same thing to the scheduler — wait indefinitely —
/// but nobody reads `grace = "0s"` and thinks "waits forever", so the word is
/// worth supporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grace(pub Duration);

impl<'de> Deserialize<'de> for Grace {
    fn deserialize<D: de::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl de::Visitor<'_> for V {
            type Value = Grace;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str(r#"a duration like "10m", or "off""#)
            }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<Grace, E> {
                if matches!(s.trim(), "off" | "never") {
                    return Ok(Grace(Duration::ZERO));
                }
                config::parse(s).map(Grace).ok_or_else(|| {
                    E::custom(format!("{s:?} is not a duration like \"10m\", or \"off\""))
                })
            }
            fn visit_i64<E: de::Error>(self, n: i64) -> Result<Grace, E> {
                // Checked, like the bound `config::parse` puts on the string
                // spelling and for the same reason: a number nobody could have
                // meant has to come back as a bad setting rather than wrap
                // silently into a grace of a few seconds.
                u64::try_from(n)
                    .ok()
                    .and_then(|n| n.checked_mul(60))
                    .map(|secs| Grace(Duration::from_secs(secs)))
                    .ok_or_else(|| E::custom(format!("{n} is not a number of minutes")))
            }
        }
        d.deserialize_any(V)
    }
}

/// Home Assistant already knows when a tag is scanned — its companion app fires
/// the event and the tag turns up as an entity — so the tidiest arrangement is
/// for tea to *ask*. Nothing here listens, nothing has to be forwarded in, and
/// the question "can the tag even be seen right now?" answers itself, because
/// this end is the one making the call.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HomeAssistant {
    /// Base address, e.g. `http://192.168.2.50:8123`.
    pub url: String,
    /// A long-lived access token: your profile page, at the bottom.
    ///
    /// A config file is a thing people paste into chat windows and commit by
    /// accident, so this can live somewhere else instead -- see `token_file`.
    pub token: String,
    /// A file holding the token, rather than the token itself.
    ///
    /// `.env` shaped: `TEA_HA_TOKEN=...`, with `#` comments and an optional
    /// `export`. A file containing nothing but the token works too, because
    /// that is what half of everyone will write. `~/` is expanded.
    pub token_file: PathBuf,
    /// What to watch. `tag.<name>` if your Home Assistant makes tag entities,
    /// otherwise any helper an automation touches when the tag is scanned.
    /// Every change of its state is read as a scan, so what kind of entity it
    /// is does not matter — a Zigbee button by the kettle would do.
    pub entity: String,
    /// How often to ask, and only while a break is on screen. Nothing is asked
    /// of Home Assistant for the other twenty-five minutes.
    pub poll: crate::config::Dur,
    /// Whose phone to poke for a fresh reading, when the gate is waiting on one.
    ///
    /// A phone reports its sensors on the companion app's own schedule -- a
    /// minute, at best -- so a break can spend its first minute waiting to hear
    /// about a walk that is already over. The way out is to ask: the Android
    /// companion app answers a notification of `command_update_sensors` by
    /// reporting everything it has, at once. Set this and tea sends one ten
    /// seconds into a break and every thirty seconds after that while the page
    /// is up, so the steps turn up on the next poll rather than on the phone's
    /// next minute.
    ///
    /// The phone's notify service -- `notify.mobile_app_<phone>`, or just
    /// `mobile_app_<phone>`. Empty pokes nobody, which is the default.
    pub nudge: String,
    /// The other direction: tea telling the hub what it is doing, so the hub
    /// can light the hall, ring the speaker, or show the next break on a
    /// dashboard. Needs only `url` and a token -- not the tag.
    pub publish: Mode,
    /// What the hub knows tea as. A sensor, so it shows up in the states list
    /// and can be graphed like anything else.
    pub publish_entity: String,
}

impl Default for HomeAssistant {
    fn default() -> Self {
        Self {
            url: String::new(),
            token: String::new(),
            token_file: PathBuf::new(),
            entity: String::new(),
            poll: crate::config::Dur(Duration::from_secs(2)),
            nudge: String::new(),
            publish: Mode::Off,
            publish_entity: "sensor.tea".into(),
        }
    }
}

/// Whether a name is shaped the way Home Assistant shapes them: a lowercase
/// domain, a dot, a lowercase name. Entity ids and service names are the same
/// shape, and both halves of both end up in a URL, which is the reason to look.
fn well_formed(domain: &str, name: &str) -> bool {
    !domain.is_empty()
        && !name.is_empty()
        && domain.chars().all(|c| c.is_ascii_lowercase() || c == '_')
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// What the token is called, in a file and in the environment.
pub const TOKEN_VAR: &str = "TEA_HA_TOKEN";

impl HomeAssistant {
    pub fn on(&self) -> bool {
        !self.url.trim().is_empty() && !self.entity.trim().is_empty()
    }

    /// Whether tea reports to the hub. Independent of the tag: a hub that only
    /// ever hears from tea, and is never asked anything, is a perfectly good
    /// arrangement for somebody who wants the hall light on during breaks.
    pub fn publishes(&self) -> bool {
        self.publish == Mode::On && !self.url.trim().is_empty()
    }

    /// Whether there is a phone to poke, and a hub to poke it through.
    pub fn nudges(&self) -> bool {
        !self.url.trim().is_empty() && !self.nudge.trim().is_empty()
    }

    /// The nudge in the two halves a service call is made of. A bare
    /// `mobile_app_pixel` is a notify service: a phone is the only thing worth
    /// poking here, and `notify.` in front of it is nobody's idea of a setting.
    pub fn nudge_call(&self) -> Option<(&str, &str)> {
        let raw = self.nudge.trim();
        (!raw.is_empty()).then(|| raw.split_once('.').unwrap_or(("notify", raw)))
    }

    /// Why publishing is on in the file and off in the process, if it is.
    pub fn publish_misconfigured(&self) -> Option<String> {
        if self.publish != Mode::On {
            return None;
        }
        if self.url.trim().is_empty() {
            return Some(
                "nfc.home_assistant.publish is on, but url is empty — nowhere to report to".into(),
            );
        }
        let entity = self.publish_entity.trim();
        if !entity.split_once('.').is_some_and(|(domain, name)| well_formed(domain, name)) {
            return Some(format!(
                "nfc.home_assistant.publish_entity {entity:?} is not an entity id like \"sensor.tea\""
            ));
        }
        None
    }

    /// Where the token actually comes from: written in the config, in a file
    /// named by it, or in the environment — in that order, most specific first.
    ///
    /// Resolved every time it is needed rather than once at startup, so editing
    /// the file and restarting is all there is to rotating it.
    pub fn secret(&self) -> Result<String, String> {
        if !self.token.trim().is_empty() {
            return Ok(self.token.trim().to_string());
        }

        if !self.token_file.as_os_str().is_empty() {
            let path = expand(&self.token_file);
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            if let Some(token) = read_env(&text, TOKEN_VAR) {
                complain_if_readable(&path);
                return Ok(token);
            }
            return Err(format!(
                "{} has no token in it — either `{TOKEN_VAR}=...` or the token on its own line",
                path.display()
            ));
        }

        std::env::var(TOKEN_VAR).map_err(|_| {
            format!(
                "no token — set nfc.home_assistant.token, or token_file, or ${TOKEN_VAR}"
            )
        })
    }

    /// Where the token is kept, for saying so without saying what it is.
    pub fn secret_source(&self) -> String {
        if !self.token.trim().is_empty() {
            "written in this file".to_string()
        } else if !self.token_file.as_os_str().is_empty() {
            expand(&self.token_file).display().to_string()
        } else {
            format!("${TOKEN_VAR}")
        }
    }

    /// How many polls apart the pokes are. The poll is the only beat there is,
    /// so thirty seconds is however many of them land nearest to thirty --
    /// fifteen at the default two, and every single one of a poll slower than
    /// the nudge itself.
    fn nudge_beats(&self) -> u32 {
        Self::beats(NUDGE_EVERY, self.every())
    }

    /// And how many polls a break waits before the first one: the ten seconds
    /// spent standing up, in the same units.
    fn nudge_first_beats(&self) -> u32 {
        Self::beats(NUDGE_FIRST, self.every())
    }

    fn beats(span: Duration, poll: Duration) -> u32 {
        ((span.as_secs_f64() / poll.as_secs_f64()).round() as u32).max(1)
    }

    /// And how long that comes to, for saying so out loud.
    pub fn nudge_every(&self) -> Duration {
        self.every() * self.nudge_beats()
    }

    /// Fast enough that the walk back is not spent waiting, slow enough that a
    /// typo cannot turn a break into a denial of service against your own hub.
    fn every(&self) -> Duration {
        self.poll.0.clamp(Duration::from_millis(500), Duration::from_secs(30))
    }
}

/// What the daemon last knew about the desk, for the server to answer with.
///
/// A tick old at worst, which is the same bargain `tea status` makes. The
/// alternative is letting a socket callback borrow the scheduler mid-tick.
#[derive(Debug, Clone, Copy, Default)]
pub struct Desk {
    pub breaking: bool,
    /// Time left on the countdown; zero once it has run out.
    pub remaining: Duration,
    /// The countdown is done and the page is waiting on the tag.
    pub waiting: bool,
    /// A scan has already been counted for this break.
    pub released: bool,
    /// The tag has been scanned for this break, whether or not that was the
    /// whole of the gate. Not the same as `released`: where a walk is counted
    /// too the tag is half of it, and anything still answering "waiting for
    /// the tag" once the tag is in sends somebody back down the hall for a
    /// thing they have already done.
    pub tag_in: bool,
    /// Steps still owed before the page will lift. Zero when the walk is done,
    /// and zero when no walk was being asked for -- the difference does not
    /// matter to anything that reads this.
    pub steps_left: u32,
}

/// The one-way letterbox between the scheduler and the server.
pub struct Link {
    desk: Cell<Desk>,
    scan: Cell<bool>,
    /// How far the walk has got. Left here by the poll, read by the tick, the
    /// same one-way arrangement as everything else in this letterbox.
    walk: Cell<Walk>,
    /// And how the time on your feet has got on, the same way.
    motion: Cell<Motion>,
    /// The jobs the page is showing, or `None` when nothing has answered yet.
    /// A `RefCell` rather than a `Cell` only because a list of strings is not
    /// `Copy`; it is the same one-way letterbox as the rest.
    board: RefCell<Option<Board>>,
    /// How many jobs have gone from open to done since this break's page went
    /// up. Read by the tick that ends the break, and reset by the poll when
    /// there is no break to count against.
    chores_done: Cell<u32>,
    /// Whether whatever watches for scans could be reached, last time it was
    /// asked. `None` until something has looked. A break cannot be gated on a
    /// signal that has no way of arriving, so this decides whether the gate
    /// applies at all -- see the engine.
    reachable: Cell<Option<bool>>,
}

impl Link {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            desk: Cell::new(Desk::default()),
            scan: Cell::new(false),
            walk: Cell::new(Walk::default()),
            motion: Cell::new(Motion::default()),
            board: RefCell::new(None),
            chores_done: Cell::new(0),
            reachable: Cell::new(None),
        })
    }

    /// The tag was scanned. Left here for the next tick to pick up, because
    /// this is called from a socket callback and the scheduler is mid-tick as
    /// often as not.
    pub fn post_scan(&self) {
        self.scan.set(true);
    }

    /// Called from the poll: this is how much of the walk has been seen.
    pub fn post_walk(&self, walk: Walk) {
        self.walk.set(walk);
    }

    /// What the walk looks like, or `None` when this break has no walk in it.
    pub fn walk(&self) -> Option<Walk> {
        let walk = self.walk.get();
        (walk.needed > 0).then_some(walk)
    }

    /// Called from the poll: this is how much time on your feet has been seen.
    pub fn post_motion(&self, motion: Motion) {
        self.motion.set(motion);
    }

    /// How the moving is going, or `None` when it is not part of this break.
    pub fn motion(&self) -> Option<Motion> {
        let motion = self.motion.get();
        (motion.needed > 0).then_some(motion)
    }

    /// Called from the poll: this is the list, as the page should show it.
    pub fn post_board(&self, board: Option<Board>) {
        *self.board.borrow_mut() = board;
    }

    /// The jobs to show, or `None` when there is no list or nothing has
    /// answered yet -- which the page treats the same way, by showing nothing.
    pub fn board(&self) -> Option<Board> {
        self.board.borrow().clone()
    }

    /// Called from the poll: this many jobs have been ticked off since the
    /// page went up.
    pub fn post_chores_done(&self, done: u32) {
        self.chores_done.set(done);
    }

    /// How many jobs this break has to its name.
    pub fn chores_done(&self) -> u32 {
        self.chores_done.get()
    }

    pub fn set_reachable(&self, ok: bool) {
        self.reachable.set(Some(ok));
    }

    pub fn reachable(&self) -> Option<bool> {
        self.reachable.get()
    }

    /// Called from the tick: this is what the desk looks like now.
    pub fn post(&self, desk: Desk) {
        self.desk.set(desk);
    }

    pub fn desk(&self) -> Desk {
        self.desk.get()
    }

    /// Called from the tick: was the tag scanned since last time?
    pub fn take_scan(&self) -> bool {
        self.scan.replace(false)
    }
}

/// A listening socket, alive for as long as this is kept.
pub struct Ear {
    // Dropping the service stops it listening, so it is held rather than
    // leaked: the engine owns the ear for as long as it runs.
    _service: gio::SocketService,
    pub addr: SocketAddr,
}

/// Open the port. The error is returned rather than fatal: a break timer that
/// refuses to start because a socket is busy would be a poor trade.
pub fn listen(cfg: &Config, link: Rc<Link>, site: Option<crate::web::Site>) -> Result<Ear, String> {
    if cfg.token.trim().is_empty() {
        return Err("port.token is empty — anyone who can reach the port could end your break \
                    or rewrite your settings (`tea settings` or `tea set-nfc on` writes one)"
            .into());
    }

    let addr: SocketAddr = cfg.listen.parse().map_err(|_| {
        format!("port.listen: {:?} is not an address:port, like \"0.0.0.0:9797\"", cfg.listen)
    })?;

    let service = gio::SocketService::new();
    service
        .add_address(
            &gio::InetSocketAddress::from(addr),
            gio::SocketType::Stream,
            gio::SocketProtocol::Tcp,
            None::<&glib::Object>,
        )
        .map_err(|e| format!("cannot listen on {addr}: {e}"))?;

    let token = Rc::new(cfg.token.clone());
    let site = Rc::new(site);
    service.connect_incoming(move |_, conn, _| {
        greet(conn.clone(), Rc::clone(&token), Rc::clone(&link), Rc::clone(&site));
        // Handled: nothing else is listening for these.
        true
    });
    service.start();

    Ok(Ear { _service: service, addr })
}

/// Read one request, answer it, hang up.
fn greet(conn: gio::SocketConnection, token: Rc<String>, link: Rc<Link>, site: Rc<Option<crate::web::Site>>) {
    let who = conn
        .remote_address()
        .ok()
        .and_then(|a| a.downcast::<gio::InetSocketAddress>().ok())
        .map(|a| a.address().to_str().to_string())
        .unwrap_or_else(|| "somewhere".to_string());

    // Nothing here waits on a client's good manners: a connection that opens
    // and then says nothing is cancelled rather than held open forever.
    let cancel = gio::Cancellable::new();
    let expire = cancel.clone();
    glib::timeout_add_local_once(READ_TIMEOUT, move || expire.cancel());

    let out = conn.output_stream();
    read_request(
        conn.input_stream(),
        Vec::new(),
        cancel,
        Box::new(move |raw| {
            // A connection that opened and never spoke gets no reply, only
            // the door closed. Chrome opens a socket the moment you start
            // typing the address and sends the request on it seconds later;
            // an answer written into that silence is what it reads back as
            // the page, and "GET or POST" is not the settings page.
            if unspoken(&raw) {
                let _ = conn.close(gio::Cancellable::NONE);
                return;
            }
            let reply = answer(&raw, &token, &link, &who, site.as_ref().as_ref());
            // Best effort: a phone that has already walked out of range is not
            // an error worth reporting, and the scan itself is already counted.
            out.write_all_async(reply.into_bytes(), glib::Priority::DEFAULT, gio::Cancellable::NONE, move |_| {
                let _ = conn.close(gio::Cancellable::NONE);
            });
        }),
    );
}

/// Accumulate until the request is all here, the cap is hit, or the client
/// stops talking. Boxed rather than generic so it can call itself.
fn read_request(
    input: gio::InputStream,
    acc: Vec<u8>,
    cancel: gio::Cancellable,
    done: Box<dyn FnOnce(Vec<u8>)>,
) {
    let more = REQUEST_CAP - acc.len();
    let next = input.clone();
    let again = cancel.clone();
    input.read_bytes_async(more, glib::Priority::DEFAULT, Some(&cancel), move |res| {
        let mut acc = acc;
        match res {
            Ok(bytes) if !bytes.is_empty() => {
                acc.extend_from_slice(&bytes);
                if complete(&acc) || acc.len() >= REQUEST_CAP {
                    done(acc);
                } else {
                    read_request(next, acc, again, done);
                }
            }
            // EOF, error, or the timeout above: answer with whatever arrived,
            // which for a well-formed request that simply lacked a blank line
            // is still enough to act on.
            _ => done(acc),
        }
    });
}

/// Whether the client never said anything at all: nothing arrived before the
/// timeout or the hang-up. Distinct from a request that arrived broken, which
/// deserves an answer saying so.
fn unspoken(raw: &[u8]) -> bool {
    raw.iter().all(u8::is_ascii_whitespace)
}

/// Where the headers stop, if they have: the index just past the blank line.
fn headers_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// What `Content-Length` promised, or nothing at all.
fn promised(head: &str) -> Option<usize> {
    head.split("\r\n")
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse().ok())
}

/// Whether everything the request said it would send has arrived: the
/// headers, and as much body as they promised. A tag's GET is complete at the
/// blank line; the settings page's POST is complete when the file is.
fn complete(raw: &[u8]) -> bool {
    match headers_end(raw) {
        Some(end) => raw.len() >= end + promised(&String::from_utf8_lossy(&raw[..end])).unwrap_or(0),
        None => false,
    }
}

/// Work out what was asked and produce the whole response.
fn answer(raw: &[u8], token: &str, link: &Link, who: &str, site: Option<&crate::web::Site>) -> String {
    let text = String::from_utf8_lossy(raw);
    let mut lines = text.split("\r\n");
    let Some(request) = lines.next() else {
        return http(400, "text/plain", "tea: nothing was asked\n");
    };

    let mut parts = request.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    if !matches!(method, "GET" | "POST" | "HEAD") {
        return http(405, "text/plain", "tea: GET or POST\n");
    }

    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let headers: Vec<(String, &str)> = lines
        .take_while(|l| !l.is_empty())
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_lowercase(), v.trim()))
        .collect();
    let header = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| *v);
    let html = header("accept").is_some_and(|a| a.contains("text/html"));

    // The tag's own URL carries the token in the query, because that is all an
    // NFC tag can do. A hub posting on your behalf can use a header instead.
    let offered = param(query, "token")
        .or_else(|| header("x-tea-token").map(str::to_string))
        .or_else(|| {
            header("authorization")
                .and_then(|a| a.strip_prefix("Bearer ").or_else(|| a.strip_prefix("bearer ")))
                .map(str::to_string)
        })
        .unwrap_or_default();

    // Behind a proxy every request arrives from the proxy, and "scan from
    // 127.0.0.1" tells you nothing on a morning when something is wrong.
    // Logging only -- a header is a claim, never an authorisation.
    let who = &caller(header("x-forwarded-for"), who);

    if !same_secret(&offered, token) {
        println!("[nfc]   refused {path} from {who} — wrong token");
        return page(html, 401, "Not this door", "That token is not the one tea is listening for.");
    }

    match path {
        "/unlock" | "/unlock/" => unlock(link, html, who),
        // The settings page, only when the file says so. The page itself is
        // harmless to hand out; the file is handed out and taken back only
        // behind the same token, which is what keeps every other page open in
        // that browser from writing a config here.
        "/settings" | "/settings/" if site.is_some() => {
            http(200, "text/html; charset=utf-8", crate::web::PAGE)
        }
        // The dashboard, on the same port and behind the same token, so the
        // two pages the daemon can show are one link apart. Built per request:
        // a chart of the last three weeks that was true when the daemon
        // started is not a chart anybody wants.
        "/dash" | "/dash/" if site.is_some() => {
            let site = site.expect("checked above");
            let page = crate::dash::page(&site.path, crate::boottime());
            http(200, "text/html; charset=utf-8", &page)
        }
        // The file as it ships, so the page can offer a setting that is not in
        // your file yet: what it would be if you never said, what the file
        // itself has to say about it, and somewhere to put it. Static text,
        // behind the same token as the file it describes.
        "/defaults" | "/defaults/" if site.is_some() => {
            http(200, "text/plain; charset=utf-8", crate::config::DEFAULT_FILE)
        }
        "/config" | "/config/" if site.is_some() => {
            let site = site.expect("checked above");
            match method {
                "POST" => {
                    // The body is the file. Not a byte less: a client that
                    // hung up early must not have half a config written in
                    // its name, and the timeout answers with whatever came.
                    let end = headers_end(raw).unwrap_or(raw.len());
                    let body = &raw[end..];
                    if promised(&String::from_utf8_lossy(&raw[..end])).is_some_and(|n| n != body.len()) {
                        return http(400, "text/plain", "tea: the file did not all arrive\n");
                    }
                    let Ok(text) = std::str::from_utf8(body) else {
                        return http(400, "text/plain", "tea: the file is not UTF-8\n");
                    };
                    // Saving and applying are two buttons on the page, and
                    // one request: the header says whether tea should
                    // restart on what it has just written.
                    let applying = header("x-tea-apply").is_some();
                    match crate::web::write(site, text) {
                        Ok(()) => {
                            println!("[web]   settings saved from {who}");
                            let note = match (applying, applying && crate::web::restart_soon()) {
                                (false, _) => "saved\n",
                                (true, true) => "saved. tea is restarting to pick it up\n",
                                (true, false) => "saved. Restart tea to pick it up\n",
                            };
                            http(200, "text/plain", note)
                        }
                        Err(e) => http(400, "text/plain; charset=utf-8", &format!("{e}\n")),
                    }
                }
                _ => match crate::web::read(site) {
                    Ok(text) => http(200, "text/plain; charset=utf-8", &text),
                    Err(e) => http(500, "text/plain; charset=utf-8", &format!("{e}\n")),
                },
            }
        }
        "/status" | "/status/" => {
            let desk = link.desk();
            let body = if !desk.breaking {
                "working\n".to_string()
            } else if desk.waiting {
                match (desk.tag_in, desk.steps_left) {
                    (false, 0) => "waiting for the tag\n".to_string(),
                    (false, n) => format!("waiting for the tag, and {n} more steps\n"),
                    (true, 0) => "waiting\n".to_string(),
                    (true, n) => format!("waiting for {n} more steps\n"),
                }
            } else {
                format!("on a break, {} left\n", human(desk.remaining))
            };
            http(200, "text/plain", &body)
        }
        _ => page(html, 404, "Nothing here", "The tag wants /unlock."),
    }
}

/// The one thing this server exists for.
fn unlock(link: &Link, html: bool, who: &str) -> String {
    let desk = link.desk();

    if !desk.breaking {
        println!("[nfc]   scan from {who} — no break to end");
        return page(
            html,
            409,
            "No break running",
            "Nothing to unlock. Scan again when the page is up.",
        );
    }

    // Post it once; the tick that follows is what actually tells the scheduler.
    link.post_scan();

    // The page the phone shows after a scan is the last chance to say that a
    // scan was not the whole of it. Somebody who walks back to the desk on the
    // strength of "Unlocked" and finds the page still up has been lied to.
    if desk.steps_left > 0 {
        let steps = desk.steps_left;
        println!("[nfc]   scan from {who} — counted, {steps} steps still to walk");
        // Only once the countdown is spent do the steps become the last thing
        // between you and your desk. Said any earlier it is a promise the page
        // will not keep: walk the twenty, come back, and find four minutes of
        // break still to run. The branch below is careful about exactly this
        // for a break with no walk in it, and this one has to be too.
        let note = match desk.waiting {
            true => format!("Counted. {steps} more steps and the page lifts — keep going."),
            false => format!(
                "Counted. {steps} more steps, and the page lifts in {}.",
                human(desk.remaining)
            ),
        };
        return page(html, 200, "Counted", &note);
    }

    if desk.waiting {
        println!("[nfc]   scan from {who} — desk unlocked");
        page(html, 200, "Unlocked", "Your desk is back. Walk slowly.")
    } else {
        let left = desk.remaining;
        let note = if desk.released {
            format!("Already counted. The page lifts in {}.", human(left))
        } else {
            format!("Counted. The page lifts on its own in {}.", human(left))
        };
        println!("[nfc]   scan from {who} — counted, {} still to run", human(left));
        page(html, 200, "Counted", &note)
    }
}

/// Who to name in the log: the phone at the far end of the proxy if one said
/// so, otherwise whatever opened the socket.
fn caller(forwarded: Option<&str>, peer: &str) -> String {
    // `X-Forwarded-For: <client>, <proxy>, <proxy>` -- the client is first.
    forwarded
        .and_then(|chain| chain.split(',').next())
        .map(str::trim)
        .filter(|first| !first.is_empty())
        .map(|first| format!("{first} (via {peer})"))
        .unwrap_or_else(|| peer.to_string())
}

/// Same length, same bytes, and no early exit on the first wrong one.
fn same_secret(offered: &str, token: &str) -> bool {
    if token.is_empty() || offered.len() != token.len() {
        return false;
    }
    offered.bytes().zip(token.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

/// One query parameter, percent-decoded.
fn param(query: &str, name: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| percent_decode(v))
}

fn percent_decode(s: &str) -> String {
    let raw = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            // Read as bytes rather than sliced out of the string. A `%` two
            // bytes before the middle of a multi-byte character used to be cut
            // there by index, which panics -- in a socket callback, before the
            // token is so much as looked at, so anything that can reach the
            // port could take the daemon down with one malformed request.
            b'%' if i + 2 < raw.len() => match hex_pair(raw[i + 1], raw[i + 2]) {
                Some(byte) => {
                    out.push(byte);
                    i += 3;
                }
                None => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    // Decoded as bytes and turned back into text once, at the end: pushing each
    // byte as its own `char` spelled every escaped multi-byte character out in
    // Latin-1, so a token with one in it never matched the token it was.
    String::from_utf8_lossy(&out).into_owned()
}

/// The byte two hex digits spell, or `None` for anything that is not two hex
/// digits. Deliberately stricter than `from_str_radix`, which also accepts a
/// leading sign: `%+5` is not an escape.
fn hex_pair(hi: u8, lo: u8) -> Option<u8> {
    fn digit(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    Some(digit(hi)? * 16 + digit(lo)?)
}

/// A phone that scanned the tag is holding a browser, and a browser showing
/// `Counted.` in Times New Roman does not read as a thing that worked.
fn page(html: bool, code: u16, title: &str, note: &str) -> String {
    if !html {
        return http(code, "text/plain", &format!("{title}: {note}\n"));
    }
    let body = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>tea — {title}</title><style>\
         html{{color-scheme:dark}}\
         body{{margin:0;min-height:100vh;display:flex;flex-direction:column;\
         align-items:center;justify-content:center;gap:.6rem;background:#0d1017;\
         color:#e6e9f0;font:400 1rem/1.5 system-ui,-apple-system,sans-serif;\
         text-align:center;padding:2rem}}\
         h1{{font:200 2.4rem/1.1 system-ui,sans-serif;margin:0}}\
         p{{margin:0;color:#79839c;max-width:22rem}}\
         </style></head><body><h1>{title}</h1><p>{note}</p></body></html>",
    );
    http(code, "text/html; charset=utf-8", &body)
}

fn http(code: u16, kind: &str, body: &str) -> String {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        500 => "Internal Server Error",
        _ => "Whatever",
    };
    format!(
        "HTTP/1.1 {code} {reason}\r\n\
         Content-Type: {kind}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
}

/// A 128-bit token, hex, from the kernel. Not a password anyone has to type —
/// it lives on the tag and in the config file, so it may as well be unguessable.
pub fn fresh_token() -> Result<String, String> {
    use std::io::Read;
    let mut file = std::fs::File::open("/dev/urandom")
        .map_err(|e| format!("cannot open /dev/urandom: {e}"))?;
    let mut buf = [0u8; 16];
    file.read_exact(&mut buf)
        .map_err(|e| format!("cannot read /dev/urandom: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

// ---------------------------------------------------------------------------
// Asking Home Assistant
// ---------------------------------------------------------------------------

/// A reply big enough for any entity, and small enough that a wrong address
/// answering with a web page cannot fill memory.
const REPLY_CAP: usize = 64 * 1024;
/// A hub that has not answered in this long is not going to.
const ASK_TIMEOUT: u32 = 10;
/// States that mean "nothing has happened yet", not "something just did.
/// Home Assistant restarts hand out `unknown` again, and reading that as a scan
/// would end a break every time the hub came back up.
const NOT_A_SCAN: &[&str] = &["unknown", "unavailable", "none", ""];

/// Everything the poll needs, shared with the timer that drives it.
struct Ask {
    host: String,
    port: u16,
    tls: bool,
    path: String,
    token: String,
    entity: String,
    /// The step sensor, when the break is also being walked off. Asked on the
    /// same beat as the tag and from the same hub, so one poll answers both
    /// halves of the gate.
    legs: Option<Legs>,
    /// The activity sensor, when the break also wants time on your feet.
    /// Same beat, same hub, third question.
    gait: Option<Gait>,
    /// The to-do list, when the page is showing one. Not on the same beat: it
    /// is asked every few of them, because a list of household jobs does not
    /// change between one second and the next, and it is the one question here
    /// that costs the hub a service call rather than a state lookup.
    jobs: Option<Jobs>,
    /// The phone, when the gate is waiting on one and the config says whose.
    /// Not a question at all -- the one thing here that tells rather than asks.
    nudge: Option<Nudge>,
    link: Rc<Link>,
    /// The entity's value when this break started. A *change* is the scan;
    /// comparing against a remembered value rather than a clock means the two
    /// machines never have to agree about what time it is.
    baseline: RefCell<Option<String>>,
    /// One question at a time. A hub that has gone slow must not end up with a
    /// queue of them.
    busy: Cell<bool>,
    /// Complain once per outage, not once per poll.
    complained: Cell<bool>,
    /// Consecutive unanswered questions about the tag, and whether there have
    /// been enough of them to call it an outage. See `MISSES_BEFORE_LOST`.
    misses: Cell<u32>,
    lost: Cell<bool>,
}

/// How many polls in a row have to go unanswered before the hub counts as
/// gone -- and the break therefore ends on its countdown.
///
/// One is far too few. A hub answers a request a second for the length of a
/// break; a single malformed reply, a dropped packet, a Home Assistant that is
/// mid-reload, and the gate that exists to make you walk quietly opens itself.
/// Three in a row at the default two-second poll is six seconds of real silence,
/// which is an outage rather than a blip.
const MISSES_BEFORE_LOST: u32 = 3;

/// The walk half of the gate: which sensor, how far, and how far you have got.
struct Legs {
    entity: String,
    path: String,
    needed: u32,
    walker: RefCell<Walker>,
    /// Its own outage latch, not the tag's: sharing one would have a hub that
    /// answers about the tag and not about the sensor announcing that it is
    /// back, once every couple of seconds, for the whole break.
    complained: Cell<bool>,
    /// And its own run of silence, for the same reason.
    misses: Cell<u32>,
    lost: Cell<bool>,
}

/// The moving half of the gate: which sensor, which of its words count, how
/// long is wanted, and how long has been seen.
///
/// Time is added up rather than clocked: every answer that says a moving word
/// is worth one poll's beat, whatever the phone got up to between. A sensor
/// that reports a minute late still reports, and the beat it is credited with
/// is the same beat a prompt one gets.
struct Gait {
    entity: String,
    path: String,
    needed: u32,
    states: Vec<String>,
    /// Seconds each moving answer is worth: the poll interval.
    beat: f64,
    secs: Cell<f64>,
    /// Its own outage latch and run of silence, for the reasons `Legs` has.
    complained: Cell<bool>,
    misses: Cell<u32>,
    lost: Cell<bool>,
}

/// The list on the wall: which list, how much of it fits, and what this break
/// has seen happen to it.
struct Jobs {
    entity: String,
    /// What the page calls the list, which is the entity id unless the config
    /// says otherwise. Kept apart from `entity`, which is what the *hub* calls
    /// it and what every error message here has to name.
    title: String,
    path: String,
    /// The service call's body, built once: it never changes.
    body: String,
    cap: usize,
    /// Beats between questions, and how many are left before the next one.
    /// Zero means ask on this one, so the first beat of a break always asks.
    every: u32,
    due: Cell<u32>,
    /// What the page is showing, fixed at the first answer of this break. The
    /// rows never move afterwards; only their statuses do.
    board: RefCell<Option<Board>>,
    /// Every job that was still open when the page went up -- the whole list,
    /// not just the rows on screen. A job ticked off during a break counts
    /// whether or not there was room to show it.
    open: RefCell<HashSet<String>>,
    /// Which of those have since been ticked off. A set rather than a counter
    /// because a job can be ticked, un-ticked and ticked again inside five
    /// minutes, and that is one job done.
    credited: RefCell<HashSet<String>>,
    /// Complain once per outage, like everything else that talks to the hub.
    complained: Cell<bool>,
}

/// The phone, and how often it is asked to speak up.
///
/// Kept out of the gate's way on purpose. A poke that fails costs a line on
/// stderr and nothing else: the worst a phone that will not be told can do is
/// leave the break exactly where it would have been without any of this --
/// waiting on the companion app's own minute, and ending on `grace` if that
/// never comes.
struct Nudge {
    /// What the hub calls it, for saying which service went wrong.
    service: String,
    path: String,
    /// Beats between pokes, beats before the first of a break, and how many are
    /// left before the next one. Each starts one short of its round, so a poke
    /// lands on the tenth second rather than the twelfth.
    every: u32,
    first: u32,
    due: Cell<u32>,
    /// One poke in flight at a time: a hub that has gone slow must not end up
    /// with a queue of orders for the phone.
    busy: Cell<bool>,
    /// Whether this break has said out loud that it is poking. Once is plenty.
    said: Cell<bool>,
    /// Complain once per outage, like everything else that talks to the hub.
    complained: Cell<bool>,
}

impl Nudge {
    /// Whether this beat is one that pokes.
    fn pokes_now(&self) -> bool {
        match self.due.get() {
            0 => {
                self.due.set(self.every.saturating_sub(1));
                true
            }
            left => {
                self.due.set(left - 1);
                false
            }
        }
    }

    /// Back to how a break starts: the next one waits its ten seconds too.
    fn forget(&self) {
        self.due.set(self.first.saturating_sub(1));
        self.said.set(false);
        self.complained.set(false);
    }
}

impl Jobs {
    /// Whether this beat is one that asks.
    fn asks_now(&self) -> bool {
        match self.due.get() {
            0 => {
                self.due.set(self.every.saturating_sub(1));
                true
            }
            left => {
                self.due.set(left - 1);
                false
            }
        }
    }

    /// The first answer of a break: lay the page out, and remember what was
    /// still open, which is what "ticked off during this break" is measured
    /// against from here on.
    fn settle_on(&self, items: Vec<Job>, today: &str) -> Board {
        *self.open.borrow_mut() = items.iter().filter(|j| !j.done).map(|j| j.uid.clone()).collect();
        self.lay_out(&items, today)
    }

    /// What the page shows, worked out from the list as it stands: the open
    /// jobs in the list's own order, then the ones most recently ticked off,
    /// and no more of either than fits.
    ///
    /// Done again on every answer, so a job ticked off from the phone moves
    /// down under the ones still open, the row it had goes to the next open
    /// job that did not fit, and the "more" marker shrinks to match. A job
    /// deleted from the list simply goes.
    ///
    /// The two are not simply concatenated and cut. A couple of lines are held
    /// back for finished jobs whenever there are any, so a list with eleven
    /// things still on it does not fill the corner with nothing but work; and
    /// when there is barely anything left to do, the finished ones take the
    /// slack rather than leaving the page half empty.
    fn lay_out(&self, items: &[Job], today: &str) -> Board {
        let (mut open, mut done): (Vec<Job>, Vec<Job>) = items.iter().cloned().partition(|j| !j.done);
        // Anything with a deadline goes to the top, soonest first; the rest
        // keep the list's own order behind it. A job that is wanted by half
        // past four is not the same kind of thing as one that is wanted.
        open.sort_by_key(|j| clock::deadline(&j.due).map_or(i64::MAX, |d| d.at));
        let today = done.iter().filter(|j| clock::day_of(&j.completed).as_deref() == Some(today)).count();

        // Freshest first. The hub writes these as UTC instants, so the string
        // order is the time order, and one without a stamp at all sorts last
        // rather than jumping the queue.
        done.sort_by(|a, b| b.completed.cmp(&a.completed));

        let held = done.len().min(CHORES_DONE_SLOTS);
        let open_rows = open.len().min(self.cap.saturating_sub(held));
        let done_rows = done.len().min(self.cap - open_rows);

        Board {
            title: self.title.clone(),
            hidden: (open.len() - open_rows) as u32,
            today: today as u32,
            jobs: open.into_iter().take(open_rows).chain(done.into_iter().take(done_rows)).collect(),
        }
    }

    /// How many of the jobs that were open when the page went up have been
    /// ticked off since. Counted across the whole list, and never uncounted:
    /// a job ticked off and then re-opened was still done.
    fn tally(&self, items: &[Job]) -> u32 {
        let open = self.open.borrow();
        let mut credited = self.credited.borrow_mut();
        for job in items.iter().filter(|j| j.done) {
            if open.contains(&job.uid) && credited.insert(job.uid.clone()) {
                println!("[ha]    ticked off — {}", job.summary);
            }
        }
        credited.len() as u32
    }

    /// Between breaks there is nothing to show and nothing to count.
    fn forget(&self) {
        *self.board.borrow_mut() = None;
        self.open.borrow_mut().clear();
        self.credited.borrow_mut().clear();
        self.due.set(0);
        self.complained.set(false);
    }
}

impl Gait {
    fn counts(&self, state: &str) -> bool {
        let state = state.trim();
        self.states.iter().any(|s| s.trim().eq_ignore_ascii_case(state))
    }

    fn motion(&self) -> Motion {
        // Rounded down, like the steps: a second that has not been walked is
        // not credited.
        Motion { secs: self.secs.get() as u32, needed: self.needed, lost: self.lost.get() }
    }

    fn forget(&self) {
        self.secs.set(0.0);
        self.complained.set(false);
        self.misses.set(0);
        self.lost.set(false);
    }
}

/// Steps taken since the break began, from a sensor that only ever reports a
/// running total.
///
/// The first reading of a break is the yardstick, never a walk in itself --
/// the same bargain the tag makes with its baseline, and for the same reason:
/// yesterday's ten thousand steps must not pay for this afternoon's break.
#[derive(Debug, Default)]
struct Walker {
    last: Option<f64>,
    walked: f64,
    /// Whether the mark being measured from is one the phone reported during
    /// this break. Until it is, a rise in the total is ground covered before
    /// the page went up: see `saw`.
    settled: bool,
    /// Whether the sensor keeps up with the walk. A live feed's first rise is
    /// the walk itself, and is credited rather than taken as the mark.
    live: bool,
}

impl Walker {
    fn new(sync: Sync) -> Self {
        Self { live: sync == Sync::Live, ..Self::default() }
    }

    /// One reading from the sensor. Says whether it was taken as this break's
    /// mark rather than credited as a walk -- worth a line in the log, because
    /// steps that visibly do not count are steps somebody walks twice.
    fn saw(&mut self, value: f64) -> bool {
        let mut yardstick = false;
        match self.last {
            // The first reading of a break is the mark to measure from, never
            // a walk in itself: yesterday's ten thousand steps must not pay
            // for this afternoon's break.
            None => {}
            Some(before) if value > before => {
                // Nor is the first *rise*, however big. A daily total reports
                // steps when the phone syncs, not when they were walked, so the
                // reading a break starts from is whatever was last synced --
                // minutes or hours old -- and everything between it and the
                // next sync covers ground from before the page went up. That is
                // the batch landing mid-break with a morning's walking in it,
                // and crediting it opens the gate from the chair, which is the
                // one thing this half of the gate exists to prevent. What comes
                // after is clean: it is measured from a total the phone
                // reported while the page was up.
                //
                // The cost is whatever was walked between the break starting
                // and the first sync after it. That is what `grace` is for.
                //
                // None of which holds for a sensor that reports as you walk.
                // There the reading a break starts from is a minute old at
                // most, and a minute before the page went up you were in the
                // chair: the first rise *is* the walk, and taking it as the
                // mark instead is the page saying *0 of 20* to somebody who
                // has just done the twenty.
                match self.settled || self.live {
                    true => self.walked += value - before,
                    false => yardstick = true,
                }
                self.settled = true;
            }
            // Backwards, which a step count never really goes. Either the
            // counter started again -- midnight, a phone that re-paired -- or
            // the total corrected itself, a duplicate source dropped or a sync
            // reconciled. Telling those apart from one reading is guesswork,
            // and guessing wrong in the generous direction credits a whole
            // day's steps at once and opens the gate from the chair. So
            // neither is credited: the new reading simply becomes the mark to
            // measure from. At a real rollover that costs the steps taken
            // between two polls, which is a couple of seconds of walking. It is
            // also a total reported during this break, so what follows it can
            // be counted.
            Some(before) if value < before => self.settled = true,
            Some(_) => {}
        }
        self.last = Some(value);
        yardstick
    }

    /// Whether the mark being measured from was laid down mid-break -- which
    /// is to say, whether a report has already been taken and not credited.
    /// Never for a live sensor: it discards nothing, so there is nothing for
    /// the page to explain.
    fn marked(&self) -> bool {
        self.settled && !self.live
    }

    fn walked(&self) -> u32 {
        // Sensors report floats, people walk in whole steps, and rounding up
        // would hand out a step nobody took.
        self.walked as u32
    }

    /// Between breaks there is nothing to count and nothing worth remembering
    /// -- except what kind of sensor this is, which does not change.
    fn forget(&mut self) {
        *self = Self { live: self.live, ..Self::default() };
    }
}

/// The poll, for as long as this is held.
pub struct Watch {
    source: Option<glib::SourceId>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        if let Some(source) = self.source.take() {
            source.remove();
        }
    }
}

/// Start asking Home Assistant about the tag, every `poll`, while a break is up.
///
/// The whole nfc config rather than just the hub: the steps sensor is asked on
/// the same beat, and splitting the two would mean two timers waking up a
/// couple of seconds apart to talk to the same machine.
pub fn watch(cfg: &Config, link: Rc<Link>) -> Result<Watch, String> {
    let ha = &cfg.home_assistant;
    let token = ha.secret().map_err(|e| {
        format!("{e} (a long-lived access token, from the bottom of your profile page)")
    })?;
    let (host, port, tls, base) = split_url(&ha.url)?;
    let entity = ha.entity.trim().to_string();

    let legs = cfg.counts_steps().then(|| {
        let entity = cfg.steps.entity.trim().to_string();
        Legs {
            path: format!("{base}/api/states/{entity}"),
            entity,
            needed: cfg.steps.count,
            walker: RefCell::new(Walker::new(cfg.steps.sync)),
            complained: Cell::new(false),
            misses: Cell::new(0),
            lost: Cell::new(false),
        }
    });
    let gait = cfg.counts_moving().then(|| {
        let entity = cfg.moving.entity.trim().to_string();
        Gait {
            path: format!("{base}/api/states/{entity}"),
            entity,
            needed: cfg.moving_secs(),
            states: cfg.moving.states.clone(),
            beat: ha.every().as_secs_f64(),
            secs: Cell::new(0.0),
            complained: Cell::new(false),
            misses: Cell::new(0),
            lost: Cell::new(false),
        }
    });
    let jobs = cfg.shows_chores().then(|| {
        let entity = cfg.chores.entity.trim().to_string();
        // A beat of its own, worked out from the poll's: at the default two
        // seconds that is every fifth one, and never less than every one.
        let every = (JOBS_EVERY.as_secs_f64() / ha.every().as_secs_f64()).round() as u32;
        Jobs {
            path: format!("{base}/api/services/todo/get_items?return_response=true"),
            body: json_body(&entity),
            cap: cfg.chores.cap(),
            title: cfg.chores.header(),
            entity,
            every: every.max(1),
            due: Cell::new(0),
            board: RefCell::new(None),
            open: RefCell::new(HashSet::new()),
            credited: RefCell::new(HashSet::new()),
            complained: Cell::new(false),
        }
    });

    // Only when something is actually waiting on the phone. Poking one whose
    // sensors nobody reads is a notification every half minute for nothing.
    let nudge = (ha.nudges() && (cfg.counts_steps() || cfg.counts_moving()))
        .then(|| ha.nudge_call())
        .flatten()
        .map(|(domain, service)| {
            let first = ha.nudge_first_beats();
            Nudge {
                path: format!("{base}/api/services/{domain}/{service}"),
                service: format!("{domain}.{service}"),
                every: ha.nudge_beats(),
                first,
                due: Cell::new(first.saturating_sub(1)),
                busy: Cell::new(false),
                said: Cell::new(false),
                complained: Cell::new(false),
            }
        });

    // Posted before the first poll so that a page built in the same tick knows
    // there is a walk in this break, rather than showing no badge for a second
    // and then growing one.
    link.post_walk(Walk {
        walked: 0,
        needed: legs.as_ref().map_or(0, |l| l.needed),
        marked: false,
    });
    link.post_motion(Motion {
        secs: 0,
        needed: gait.as_ref().map_or(0, |g| g.needed),
        lost: false,
    });

    let ask = Rc::new(Ask {
        host,
        port,
        tls,
        path: format!("{base}/api/states/{entity}"),
        token,
        entity,
        legs,
        gait,
        jobs,
        nudge,
        link,
        baseline: RefCell::new(None),
        busy: Cell::new(false),
        complained: Cell::new(false),
        misses: Cell::new(0),
        lost: Cell::new(false),
    });

    let source = glib::timeout_add_local(ha.every(), move || {
        // Only while the page is up. Between breaks there is nothing a scan
        // could mean, and a hub polled all day for no reason is a hub whose
        // owner turns this off.
        if !ask.link.desk().breaking {
            *ask.baseline.borrow_mut() = None;
            ask.complained.set(false);
            ask.misses.set(0);
            ask.lost.set(false);
            ask.link.reachable.set(None);
            if let Some(legs) = &ask.legs {
                legs.walker.borrow_mut().forget();
                legs.complained.set(false);
                legs.misses.set(0);
                legs.lost.set(false);
                ask.link.post_walk(Walk { walked: 0, needed: legs.needed, marked: false });
            }
            if let Some(gait) = &ask.gait {
                gait.forget();
                ask.link.post_motion(gait.motion());
            }
            // The next break gets the list as it stands then, and starts
            // counting what gets ticked off from zero.
            if let Some(jobs) = &ask.jobs {
                jobs.forget();
                ask.link.post_board(None);
                ask.link.post_chores_done(0);
            }
            if let Some(nudge) = &ask.nudge {
                nudge.forget();
            }
            return glib::ControlFlow::Continue;
        }
        poll(Rc::clone(&ask));
        // Its own errand rather than one more question on the poll's string of
        // them: the gate's answers must not wait behind a notification.
        poke(Rc::clone(&ask));
        glib::ControlFlow::Continue
    });

    Ok(Watch { source: Some(source) })
}

/// Ask once, now, and hand back whatever the hub says — for `tea --probe`,
/// which is where a mistyped token or entity name gets caught before it becomes
/// a break page that will not lift.
pub fn probe(cfg: &HomeAssistant, entity: &str) -> Result<String, String> {
    let token = cfg.secret()?;
    let (host, port, tls, base) = split_url(&cfg.url)?;
    let entity = entity.trim().to_string();
    let ask = Ask {
        path: format!("{base}/api/states/{entity}"),
        host,
        port,
        tls,
        token,
        entity,
        legs: None,
        gait: None,
        jobs: None,
        nudge: None,
        link: Link::new(),
        baseline: RefCell::new(None),
        busy: Cell::new(false),
        complained: Cell::new(false),
        misses: Cell::new(0),
        lost: Cell::new(false),
    };
    let path = ask.path.clone();
    let entity = ask.entity.clone();
    glib::MainContext::default().block_on(fetch(&ask, &path, &entity))
}

/// One question, asked on the main loop.
///
/// `spawn_local` rather than a thread: this runs on the same context as the
/// scheduler and the pages, so there is still nothing to lock, and an answer
/// that arrives mid-tick simply waits its turn like every other event.
fn poll(ask: Rc<Ask>) {
    if ask.busy.replace(true) {
        return;
    }
    glib::MainContext::default().spawn_local(async move {
        let tag = fetch(&ask, &ask.path, &ask.entity).await;
        // One after the other, not both at once: two sockets to the same hub
        // every couple of seconds, for a number that changes at walking pace,
        // is not a trade worth making.
        let steps = match &ask.legs {
            Some(legs) => Some(fetch(&ask, &legs.path, &legs.entity).await),
            None => None,
        };
        let moving = match &ask.gait {
            Some(gait) => Some(fetch(&ask, &gait.path, &gait.entity).await),
            None => None,
        };
        // Last, and not on every beat: the gate's three questions are what the
        // break actually turns on, and none of them should queue behind a list
        // of household jobs.
        let jobs = match &ask.jobs {
            Some(jobs) if jobs.asks_now() => Some(ask_jobs(&ask, jobs).await),
            _ => None,
        };
        ask.busy.set(false);
        settle(&ask, tag, steps, moving, jobs);
    });
}

/// Tell the phone to report itself, now.
///
/// The one thing in this module that talks rather than listens to the gate, and
/// the only one whose failure changes nothing: a phone that will not be told
/// leaves the break where it already was, waiting on the companion app's own
/// minute. So this never touches `reachable`, never counts a miss, and never
/// ends or holds a break -- it costs a line on stderr, once, and the poll gets
/// on with asking.
///
/// Off the poll's string of questions rather than on the end of it, because
/// those are what the break actually turns on: a notification to a phone must
/// never be the thing a step count is queued behind.
fn poke(ask: Rc<Ask>) {
    match &ask.nudge {
        // Still telling it about the last beat: let this one go by rather than
        // stack up a second order behind the first.
        Some(nudge) if wanted(&ask) && !nudge.busy.get() && nudge.pokes_now() => {
            nudge.busy.set(true)
        }
        _ => return,
    }
    glib::MainContext::default().spawn_local(async move {
        let Some(nudge) = &ask.nudge else { return };
        if !nudge.said.replace(true) {
            println!("[ha]    asking {} for a fresh reading", nudge.service);
        }
        let answer = send(&ask, &nudge.path, Some(NUDGE_BODY), NUDGE_TIMEOUT).await;
        nudge.busy.set(false);
        settle_poke(nudge, answer.and_then(|raw| read_poke(&raw, &nudge.service)));
    });
}

async fn fetch(ask: &Ask, path: &str, entity: &str) -> Result<String, String> {
    let raw = send(ask, path, None, ASK_TIMEOUT).await?;
    read_state(&raw, entity)
}

/// The list, as the hub currently has it.
///
/// A service call rather than a state lookup, because a to-do entity's state is
/// the *number* of things left on it and nothing else: the lines themselves only
/// come back from `todo.get_items`, which is a POST with a body and a reply
/// worth parsing. Given its own timeout, and a short one -- this is decoration
/// on a page that has a job to do, and it must never be what the poll is
/// waiting for.
async fn ask_jobs(ask: &Ask, jobs: &Jobs) -> Result<Vec<Job>, String> {
    let raw = send(ask, &jobs.path, Some(&jobs.body), JOBS_TIMEOUT).await?;
    read_jobs(&raw, &jobs.entity)
}

/// One request, on the main loop. A `body` makes it a POST.
async fn send(ask: &Ask, path: &str, body: Option<&str>, timeout: u32) -> Result<Vec<u8>, String> {
    let client = gio::SocketClient::new();
    client.set_tls(ask.tls);
    client.set_timeout(timeout);

    let conn = client
        .connect_to_host_future(&format!("{}:{}", ask.host, ask.port), ask.port)
        .await
        .map_err(|e| format!("cannot reach {}:{} — {e}", ask.host, ask.port))?;

    // HTTP/1.0 on purpose: it cannot be answered with a chunked body, which
    // saves unpicking one for the sake of a forty-byte string.
    let request = match body {
        None => format!(
            "GET {} HTTP/1.0\r\nHost: {}\r\nAuthorization: Bearer {}\r\n\
             Accept: application/json\r\nConnection: close\r\n\r\n",
            path, ask.host, ask.token
        ),
        Some(body) => format!(
            "POST {} HTTP/1.0\r\nHost: {}\r\nAuthorization: Bearer {}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\
             Accept: application/json\r\nConnection: close\r\n\r\n{}",
            path,
            ask.host,
            ask.token,
            body.len(),
            body
        ),
    };
    conn.output_stream()
        .write_all_future(request.into_bytes(), glib::Priority::DEFAULT)
        .await
        .map_err(|(_, e)| format!("cannot ask: {e}"))?;

    let input = conn.input_stream();
    let mut raw: Vec<u8> = Vec::new();
    loop {
        let chunk = input
            .read_bytes_future(8192, glib::Priority::DEFAULT)
            .await
            .map_err(|e| format!("no answer: {e}"))?;
        if chunk.is_empty() || raw.len() >= REPLY_CAP {
            break;
        }
        raw.extend_from_slice(&chunk);
    }
    let _ = conn.close(gio::Cancellable::NONE);

    Ok(raw)
}

/// Pull the entity's state out of the reply, and say something useful about
/// every way it can go wrong — this is the setup people get wrong, and "it
/// didn't work" is not a thing anybody can act on.
fn read_state(raw: &[u8], entity: &str) -> Result<String, String> {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| "Home Assistant answered with something that is not HTTP".to_string())?;
    let status = head.lines().next().unwrap_or("").split_whitespace().nth(1).unwrap_or("");

    match status {
        "200" => {}
        "401" | "403" => {
            return Err("Home Assistant refused the token (nfc.home_assistant.token)".into());
        }
        "404" => {
            return Err(format!("Home Assistant has no entity called {entity:?}"));
        }
        other => return Err(format!("Home Assistant answered {other}")),
    }

    #[derive(Deserialize)]
    struct Reply {
        state: String,
    }
    serde_json::from_str::<Reply>(body.trim())
        .map(|reply| reply.state)
        .map_err(|e| format!("cannot read the answer about {entity}: {e}"))
}

/// Whether the phone is still worth waking: some half of the gate is waiting on
/// a sensor of its.
///
/// The walk usually comes in with minutes of the break still to run, and every
/// poke after that is a notification asking about a number nothing reads any
/// more. A count that goes *down* -- midnight, a re-paired phone -- moves the
/// mark and starts the walk over, and the poking starts again with it.
fn wanted(ask: &Ask) -> bool {
    let walking = ask.legs.is_some() && !ask.link.walk().is_some_and(|walk| walk.done());
    let moving = ask.gait.is_some() && !ask.link.motion().is_some_and(|motion| motion.done());
    walking || moving
}

/// Whether the hub took the order to poke the phone.
///
/// There is nothing to read in the reply: a notify service answers with an
/// empty list whatever the phone does about it, and what it does about it turns
/// up as a step count or does not turn up at all. Only the status line matters,
/// and only so that a name nobody can call is said out loud once rather than
/// failing quietly every half minute for the rest of the day.
fn read_poke(raw: &[u8], service: &str) -> Result<(), String> {
    let text = String::from_utf8_lossy(raw);
    let (head, _) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| "Home Assistant answered with something that is not HTTP".to_string())?;
    let status = head.lines().next().unwrap_or("").split_whitespace().nth(1).unwrap_or("");

    match status {
        "200" | "201" => Ok(()),
        "401" | "403" => Err("Home Assistant refused the token (nfc.home_assistant.token)".into()),
        // What a hub says about a phone it has never met. The service is the
        // companion app's registration by another name, so it is missing for
        // the same reasons the app is: never set up, renamed, or logged out.
        "400" | "404" => Err(format!(
            "Home Assistant has no {service} service — that is the phone's name in the \
             companion app (nfc.home_assistant.nudge)"
        )),
        other => Err(format!("Home Assistant answered {other}")),
    }
}

/// Pull the lines out of a `todo.get_items` reply.
///
/// The shape is `{"service_response": {"<entity>": {"items": [...]}}}`, and
/// each item is a `summary` and a `status` of `needs_action` or `completed`.
/// Anything else in there -- due dates, descriptions, the completion time -- is
/// deliberately dropped: the page shows a line and whether it is struck
/// through, and a field nobody reads is a field that can go wrong.
fn read_jobs(raw: &[u8], entity: &str) -> Result<Vec<Job>, String> {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| "Home Assistant answered with something that is not HTTP".to_string())?;
    let status = head.lines().next().unwrap_or("").split_whitespace().nth(1).unwrap_or("");

    match status {
        "200" => {}
        "401" | "403" => {
            return Err("Home Assistant refused the token (nfc.home_assistant.token)".into());
        }
        // What a service call says about an entity that is not there, or is
        // not a list at all. The state lookup's 404 never applies here: the
        // service exists whether or not your entity does.
        "400" => {
            return Err(format!(
                "Home Assistant will not read {entity:?} as a to-do list (nfc.chores.entity)"
            ));
        }
        "404" => {
            return Err(
                "Home Assistant has no todo.get_items service — the To-do list integration is \
                 not set up on that hub"
                    .into(),
            );
        }
        other => return Err(format!("Home Assistant answered {other}")),
    }

    #[derive(Deserialize)]
    struct Reply {
        service_response: std::collections::HashMap<String, Items>,
    }
    #[derive(Deserialize)]
    struct Items {
        items: Vec<Item>,
    }
    #[derive(Deserialize)]
    struct Item {
        #[serde(default)]
        uid: String,
        #[serde(default)]
        summary: String,
        #[serde(default)]
        status: String,
        #[serde(default)]
        completed: String,
        #[serde(default)]
        due: String,
    }

    let reply: Reply = serde_json::from_str(body.trim())
        .map_err(|e| format!("cannot read the list {entity}: {e}"))?;
    // Keyed by entity id, and asked for by entity id, so this is the only key
    // in it -- but taking whatever is there rather than insisting on the name
    // costs nothing and survives a hub that answers about `Todo.X`.
    let items = reply
        .service_response
        .into_values()
        .next()
        .ok_or_else(|| format!("Home Assistant said nothing about {entity}"))?;

    Ok(items
        .items
        .into_iter()
        .filter(|item| !item.summary.trim().is_empty())
        .map(|item| Job {
            // A list kept somewhere that hands out no uids would have every
            // row looking like every other one. The summary is the fallback
            // identity: not unique in principle, unique in every real list.
            uid: match item.uid.trim().is_empty() {
                true => item.summary.trim().to_string(),
                false => item.uid,
            },
            summary: item.summary.trim().to_string(),
            done: item.status.trim() == "completed",
            completed: item.completed,
            due: item.due,
        })
        .collect())
}

/// What one answer means for the break on screen.
fn settle(
    ask: &Ask,
    answer: Result<String, String>,
    steps: Option<Result<String, String>>,
    moving: Option<Result<String, String>>,
    jobs: Option<Result<Vec<Job>, String>>,
) {
    settle_tag(ask, answer);
    if let (Some(legs), Some(answer)) = (&ask.legs, steps) {
        settle_steps(ask, legs, answer);
    }
    if let (Some(gait), Some(answer)) = (&ask.gait, moving) {
        settle_motion(ask, gait, answer);
    }
    if let (Some(list), Some(answer)) = (&ask.jobs, jobs) {
        settle_jobs(ask, list, answer);
    }
}

/// What came back about the list.
///
/// Nothing in here touches `reachable`, and that is the whole design: the list
/// is not part of the gate. A hub that cannot be asked about it must not end a
/// break early, must not hold one open, and must not put an error on a page
/// whose one job is to be restful. It costs a line on stderr, once, and the
/// panel keeps saying whatever it last said.
fn settle_jobs(ask: &Ask, jobs: &Jobs, answer: Result<Vec<Job>, String>) {
    let items = match answer {
        Ok(items) => items,
        Err(why) => {
            if !jobs.complained.replace(true) {
                eprintln!("tea: cannot read the to-do list — {why}");
            }
            return;
        }
    };
    if jobs.complained.replace(false) {
        println!("[ha]    the to-do list is answering again");
    }

    let today = clock::today();
    let mut held = jobs.board.borrow_mut();
    let board = match held.is_some() {
        // Every answer after the first: the same list, laid out again, so
        // what has just been ticked off drops under what has not.
        true => jobs.lay_out(&items, &today),
        // The first of this break also fixes what "ticked off during this
        // break" is measured against.
        false => jobs.settle_on(items.clone(), &today),
    };
    *held = Some(board.clone());
    drop(held);

    ask.link.post_chores_done(jobs.tally(&items));
    ask.link.post_board((!board.jobs.is_empty()).then_some(board));
}

/// What came back from poking the phone. Nothing but a line on stderr either
/// way: see `poke` for why this is the one answer here that changes nothing.
fn settle_poke(nudge: &Nudge, answer: Result<(), String>) {
    match answer {
        Ok(()) => {
            if nudge.complained.replace(false) {
                println!("[ha]    {} is taking the hint again", nudge.service);
            }
        }
        Err(why) => {
            if !nudge.complained.replace(true) {
                eprintln!("tea: cannot ask {} for a fresh reading — {why}", nudge.service);
            }
        }
    }
}

/// The service call's body. Hand-built rather than pulled through `serde_json`
/// for one field, and the entity is quoted properly because an entity id from
/// a config file is not a thing to paste into JSON unescaped.
fn json_body(entity: &str) -> String {
    let mut out = String::from("{\"entity_id\":\"");
    for c in entity.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 => out.push(' '),
            c => out.push(c),
        }
    }
    out.push_str("\"}");
    out
}

/// The moving half. Every answer in a moving state is worth one beat of the
/// poll; anything else -- a still phone, `unknown`, a sensor that has not
/// reported yet -- leaves the count where it was. Nothing here ever takes
/// time away: a phone that says `still` for a second between two `walking`s
/// has not undone the walk.
fn settle_motion(ask: &Ask, gait: &Gait, answer: Result<String, String>) {
    match answer {
        Ok(state) => {
            gait.misses.set(0);
            if gait.lost.replace(false) {
                ask.link.post_motion(gait.motion());
            }
            if gait.complained.replace(false) {
                println!("[ha]    the activity is answering again");
            }
            if !ask.lost.get() {
                ask.link.set_reachable(true);
            }
            if gait.counts(&state) {
                let before = gait.motion();
                gait.secs.set(gait.secs.get() + gait.beat);
                let now = gait.motion();
                if now.secs != before.secs {
                    match now.left() {
                        0 if !before.done() => println!("[ha]    {}s on your feet — that's the moving", now.secs),
                        0 => {}
                        left => println!("[ha]    {}s on your feet, {left}s to go", now.secs),
                    }
                    ask.link.post_motion(now);
                }
            }
        }
        Err(why) => {
            // Same bargain as the steps: a sensor that cannot be read is a
            // half of the gate that will never close, so once a run of misses
            // says it is gone rather than slow, the engine is told and the
            // break ends on the clock.
            gait.misses.set(gait.misses.get() + 1);
            if gait.misses.get() < MISSES_BEFORE_LOST {
                return;
            }
            gait.lost.set(true);
            ask.link.post_motion(gait.motion());
            if !gait.complained.replace(true) {
                eprintln!("tea: cannot ask Home Assistant whether you are moving — {why}");
            }
            ask.link.set_reachable(false);
        }
    }
}

/// The walk half. Anything the sensor cannot answer for leaves the count where
/// it was: a hub that goes quiet mid-break must not undo steps already walked,
/// and a sensor that says `unknown` has not said zero.
fn settle_steps(ask: &Ask, legs: &Legs, answer: Result<String, String>) {
    match answer {
        Ok(value) => {
            legs.misses.set(0);
            legs.lost.set(false);
            if legs.complained.replace(false) {
                println!("[ha]    the step count is answering again");
            }
            // Only this half was ever in doubt, so only this half clears it.
            if !ask.lost.get() {
                ask.link.set_reachable(true);
            }
            if let Ok(count) = value.trim().parse::<f64>()
                && count.is_finite()
            {
                let mut walker = legs.walker.borrow_mut();
                let before = walker.walked();
                let yardstick = walker.saw(count);
                let walked = walker.walked();
                if yardstick {
                    println!("[ha]    the step count caught up — the walk counts from here");
                }
                let marked = walker.marked();
                if walked != before {
                    let left = legs.needed.saturating_sub(walked);
                    match left {
                        0 if before < legs.needed => println!("[ha]    {walked} steps — that's the walk"),
                        0 => {}
                        left => println!("[ha]    {walked} steps, {left} to go"),
                    }
                }
                ask.link.post_walk(Walk { walked, needed: legs.needed, marked });
            }
            // A sensor that has nothing to say yet (`unknown`, `unavailable`,
            // a phone that has not synced) is not an error and not a zero. It
            // is the reason `grace` exists: the break ends on the clock rather
            // than on a step count that is never going to arrive.
        }
        Err(why) => {
            // Unlike the tag, this one cannot be worked around by walking to
            // the hall and trying again: if the step sensor cannot be read, the
            // gate has a half that will never close. Say the source is gone,
            // which is what the engine reads to hand the desk back -- but only
            // once a run of them says it is gone rather than slow.
            legs.misses.set(legs.misses.get() + 1);
            if legs.misses.get() < MISSES_BEFORE_LOST {
                return;
            }
            legs.lost.set(true);
            if !legs.complained.replace(true) {
                eprintln!("tea: cannot ask Home Assistant about your steps — {why}");
            }
            ask.link.set_reachable(false);
        }
    }
}

/// The tag half.
fn settle_tag(ask: &Ask, answer: Result<String, String>) {
    let value = match answer {
        Ok(value) => value,
        Err(why) => {
            // Not fatal, and not even unusual -- a hub reboots, a laptop moves
            // to another network, a reply arrives malformed. It matters only
            // because a gate nobody can open is a gate that has to come off,
            // which the engine sees to -- so it takes a run of silence rather
            // than one bad answer to say so. One is a packet; three is a hub.
            ask.misses.set(ask.misses.get() + 1);
            if ask.misses.get() < MISSES_BEFORE_LOST {
                return;
            }
            ask.lost.set(true);
            if !ask.complained.replace(true) {
                eprintln!("tea: cannot ask Home Assistant about the tag — {why}");
            }
            ask.link.set_reachable(false);
            return;
        }
    };

    ask.misses.set(0);
    ask.lost.set(false);
    if ask.complained.replace(false) {
        println!("[ha]    Home Assistant is answering again");
    }
    // The other half may still be out; saying the hub is there when the step
    // sensor is not would paint a gate that cannot close as a working one.
    if !ask.legs.as_ref().is_some_and(|legs| legs.lost.get()) {
        ask.link.set_reachable(true);
    }

    let state = value.trim().to_ascii_lowercase();
    let blank = NOT_A_SCAN.contains(&state.as_str());

    let mut baseline = ask.baseline.borrow_mut();
    let Some(before) = baseline.as_deref() else {
        // First look of this break: whatever it says now is what "not scanned
        // yet" looks like. Never a scan in itself -- otherwise a tag touched at
        // three o'clock would pay for the four o'clock break. `unavailable` is
        // not even a look -- the watcher is down, and whatever it recovers to
        // is old news rather than a walk to the hall.
        if state != "unavailable" {
            *baseline = Some(value);
        }
        return;
    };

    if !blank && value != before {
        println!("[ha]    {} changed — the tag was scanned", ask.entity);
        ask.link.post_scan();
        *baseline = Some(value);
    }
    // A blank never moves the yardstick: an entity that says `unavailable` or
    // `unknown` mid-break and then recovers to the state it already had must
    // not read as a scan the moment it comes back.
}

/// One name out of a `.env` file. A file with no assignments in it at all is
/// taken as the token itself, because that is what people write when they are
/// told to put a secret in a file.
fn read_env(text: &str, want: &str) -> Option<String> {
    let mut lone = None;
    let mut assigned = false;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").map(str::trim).unwrap_or(line);
        match line.split_once('=') {
            Some((name, value)) => {
                assigned = true;
                if name.trim() == want {
                    return Some(unquote(value.trim()).to_string());
                }
            }
            None if lone.is_none() => lone = Some(line),
            None => {}
        }
    }

    (!assigned).then_some(lone).flatten().map(str::to_string)
}

fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value.strip_prefix(quote).and_then(|v| v.strip_suffix(quote)) {
            return inner;
        }
    }
    value
}

/// `~/` is what people write, and what nothing but a shell expands.
fn expand(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => path.to_path_buf(),
        },
        None => path.to_path_buf(),
    }
}

/// Moving a secret out of the config file and into one everybody can read is
/// not moving it anywhere. Said once, on the way past, never fatal.
fn complain_if_readable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode();
        if mode & 0o077 != 0 {
            eprintln!(
                "tea: {} is readable by others ({:o}) — chmod 600 it",
                path.display(),
                mode & 0o777
            );
        }
    }
}

/// `http://host:8123`, `https://ha.example/hass` — scheme, host, port, prefix.
pub(crate) fn split_url(url: &str) -> Result<(String, u16, bool, String), String> {
    let raw = url.trim().trim_end_matches('/');
    let (tls, rest) = match raw {
        _ if raw.starts_with("https://") => (true, &raw[8..]),
        _ if raw.starts_with("http://") => (false, &raw[7..]),
        // No scheme is a mistake worth naming rather than guessing at.
        _ => {
            return Err(format!(
                "nfc.home_assistant.url: {url:?} needs to start with http:// or https://"
            ));
        }
    };

    let (authority, prefix) = match rest.find('/') {
        Some(cut) => (&rest[..cut], rest[cut..].trim_end_matches('/').to_string()),
        None => (rest, String::new()),
    };
    if authority.is_empty() {
        return Err(format!("nfc.home_assistant.url: {url:?} has no address in it"));
    }

    // `[::1]:8123` keeps its brackets; a bare `host:port` splits at the colon.
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !host.ends_with(']') || port.chars().all(|c| c.is_ascii_digit()) => {
            let port = port
                .parse()
                .map_err(|_| format!("nfc.home_assistant.url: {port:?} is not a port"))?;
            (host.to_string(), port)
        }
        _ => (authority.to_string(), if tls { 443 } else { 80 }),
    };

    Ok((host, port, tls, prefix))
}

/// Knock on the running daemon's door yourself.
///
/// The escape hatch, and the way to try the whole thing without leaving your
/// chair — which is also exactly why it is a command you have to type rather
/// than a button on the page.
pub fn knock(cfg: &Config) -> Result<String, String> {
    use std::io::{Read, Write};

    let addr: SocketAddr = cfg
        .listen
        .parse()
        .map_err(|_| format!("port.listen: {:?} is not an address:port", cfg.listen))?;
    // 0.0.0.0 is where it listens, not somewhere anything can connect to.
    let target = if addr.ip().is_unspecified() {
        SocketAddr::from(([127, 0, 0, 1], addr.port()))
    } else {
        addr
    };

    let mut sock = std::net::TcpStream::connect_timeout(&target, Duration::from_secs(3))
        .map_err(|e| format!("cannot reach tea at {target}: {e} (is the service running?)"))?;
    let _ = sock.set_read_timeout(Some(Duration::from_secs(3)));
    // In a header rather than the query string, so a token full of awkward
    // characters needs no escaping on the way out.
    let request = format!(
        "GET /unlock HTTP/1.1\r\nHost: {target}\r\nX-Tea-Token: {}\r\n\
         Connection: close\r\n\r\n",
        cfg.token
    );
    sock.write_all(request.as_bytes()).map_err(|e| format!("cannot ask: {e}"))?;

    let mut reply = String::new();
    sock.read_to_string(&mut reply).map_err(|e| format!("no answer: {e}"))?;
    let body = reply.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or(&reply);
    Ok(body.trim().to_string())
}

/// The address a phone on the same network would have to use. Found by asking
/// the routing table which source address it would pick — no packet is sent,
/// and nothing is resolved.
pub fn lan_address() -> Option<String> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("192.0.2.1:9").ok()?;
    Some(sock.local_addr().ok()?.ip().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link_with(desk: Desk) -> Rc<Link> {
        let link = Link::new();
        link.post(desk);
        link
    }

    fn get(link: &Link, target: &str) -> String {
        answer(format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes(), "s3cret", link, "test", None)
    }

    #[test]
    fn a_connection_that_never_spoke_is_closed_without_a_word() {
        // The browser's speculative socket: opened early, silent until the
        // real request, which must not find a 405 already waiting for it.
        assert!(unspoken(b""));
        assert!(unspoken(b"\r\n"));
        // Anything actually said is answered, however badly it went.
        assert!(!unspoken(b"G"));
        assert!(!unspoken(b"GET /settings HTTP/1.1\r\n"));
        assert!(answer(b"BREW /settings HTTP/1.1\r\n\r\n", "s3cret", &Link::new(), "test", None)
            .starts_with("HTTP/1.1 405"));
    }

    #[test]
    fn the_right_token_on_a_waiting_break_unlocks_it() {
        let link = link_with(Desk { breaking: true, waiting: true, ..Desk::default() });
        assert!(get(&link, "/unlock?token=s3cret").starts_with("HTTP/1.1 200"));
        assert!(link.take_scan(), "the tick has a scan waiting for it");
    }

    #[test]
    fn a_malformed_escape_is_answered_rather_than_fatal() {
        // A `%` followed by a byte that is not UTF-8 used to be sliced by byte
        // index and panic -- inside a socket callback, so the daemon went with
        // it, and before the token was checked, so anyone who could reach the
        // port could do it.
        let link = link_with(Desk { breaking: true, waiting: true, ..Desk::default() });
        let mut raw = b"GET /unlock?token=%a".to_vec();
        raw.push(0xFF);
        raw.extend_from_slice(b" HTTP/1.1\r\nHost: x\r\n\r\n");

        assert!(answer(&raw, "s3cret", &link, "test", None).starts_with("HTTP/1.1 401"));
        assert!(!link.take_scan(), "and it is still not a way in");
    }

    #[test]
    fn an_escaped_token_decodes_to_the_token() {
        assert_eq!(percent_decode("s3cret"), "s3cret");
        assert_eq!(percent_decode("s%33cret"), "s3cret", "an escape anybody's phone might send");
        assert_eq!(percent_decode("a+b"), "a b");

        // Whole characters, not one Latin-1 char per byte: a token with a
        // multi-byte character in it has to come back as what was sent.
        assert_eq!(percent_decode("caf%C3%A9"), "café");

        // Anything that is not two hex digits is a literal percent sign, and
        // none of these may panic.
        for (raw, want) in [("100%", "100%"), ("%zz", "%zz"), ("%a", "%a"), ("%", "%")] {
            assert_eq!(percent_decode(raw), want, "{raw:?}");
        }
    }

    #[test]
    fn a_grace_of_absurd_minutes_is_refused_rather_than_wrapped() {
        let grace = |line: &str| toml::from_str::<Config>(line).map(|c| c.grace);

        // A bare number is minutes, and ten of them is ten of them.
        assert_eq!(grace("grace = 10").unwrap(), Grace(Duration::from_secs(600)));

        // 2^63-1 minutes overflows the seconds it would be. That used to wrap
        // into a grace of moments -- a break that hands the desk straight back
        // -- rather than report a setting nobody could have meant.
        let huge = grace(&format!("grace = {}", i64::MAX));
        assert!(huge.is_err(), "{huge:?}");
        assert!(grace("grace = -1").is_err(), "and neither is a negative one");
    }

    #[test]
    fn a_wrong_token_is_refused_and_posts_nothing() {
        let link = link_with(Desk { breaking: true, waiting: true, ..Desk::default() });
        assert!(get(&link, "/unlock?token=guess").starts_with("HTTP/1.1 401"));
        assert!(get(&link, "/unlock").starts_with("HTTP/1.1 401"), "and no token at all");
        assert!(!link.take_scan(), "a stranger must not be able to end a break");
    }

    #[test]
    fn a_scan_with_no_break_running_is_not_banked_for_the_next_one() {
        // Otherwise a tag scanned on the way past at 3pm would silently pay for
        // the 4pm break, which is the one thing the walk is supposed to prove.
        let link = link_with(Desk::default());
        assert!(get(&link, "/unlock?token=s3cret").starts_with("HTTP/1.1 409"));
        assert!(!link.take_scan());
    }

    #[test]
    fn a_header_token_works_too_for_anything_that_is_not_a_tag() {
        let link = link_with(Desk { breaking: true, waiting: true, ..Desk::default() });
        let raw = "POST /unlock HTTP/1.1\r\nAuthorization: Bearer s3cret\r\n\r\n";
        assert!(answer(raw.as_bytes(), "s3cret", &link, "test", None).starts_with("HTTP/1.1 200"));
    }

    #[test]
    fn a_browser_gets_a_page_and_a_hub_gets_a_line() {
        let link = link_with(Desk { breaking: true, waiting: true, ..Desk::default() });
        let phone = "GET /unlock?token=s3cret HTTP/1.1\r\nAccept: text/html,*/*\r\n\r\n";
        assert!(answer(phone.as_bytes(), "s3cret", &link, "test", None).contains("<!doctype html>"));
        assert!(get(&link, "/unlock?token=s3cret").contains("text/plain"));
    }

    #[test]
    fn a_break_still_running_says_how_long_is_left() {
        let link = link_with(Desk {
            breaking: true,
            remaining: Duration::from_secs(190),
            ..Desk::default()
        });
        let reply = get(&link, "/unlock?token=s3cret");
        assert!(reply.starts_with("HTTP/1.1 200"));
        assert!(reply.contains("3m10s"), "{reply}");
        assert!(link.take_scan(), "an early scan still counts");
    }

    #[test]
    fn the_phone_is_never_told_the_page_lifts_when_it_does_not() {
        // Steps still owed, but the countdown has minutes to run: walking them
        // off does not give the desk back, and a page that says it does sends
        // somebody back to the chair to find the break still up.
        let early = link_with(Desk {
            breaking: true,
            remaining: Duration::from_secs(190),
            steps_left: 20,
            ..Desk::default()
        });
        let reply = get(&early, "/unlock?token=s3cret");
        assert!(reply.contains("20 more steps"), "{reply}");
        assert!(reply.contains("3m10s"), "the countdown is the other half: {reply}");

        // Once the time is served the steps really are the last of it.
        let waiting = link_with(Desk {
            breaking: true,
            waiting: true,
            steps_left: 20,
            ..Desk::default()
        });
        let reply = get(&waiting, "/unlock?token=s3cret");
        assert!(reply.contains("20 more steps and the page lifts"), "{reply}");
    }

    #[test]
    fn status_stops_asking_for_a_tag_that_is_already_in() {
        let desk = Desk { breaking: true, waiting: true, steps_left: 8, ..Desk::default() };
        assert!(get(&link_with(desk), "/status?token=s3cret").contains("waiting for the tag, and 8"));

        let scanned = Desk { tag_in: true, ..desk };
        let reply = get(&link_with(scanned), "/status?token=s3cret");
        assert!(reply.contains("waiting for 8 more steps"), "{reply}");
        assert!(!reply.contains("the tag"), "the tag is in — stop asking for it: {reply}");
    }

    #[test]
    fn junk_gets_a_refusal_rather_than_a_panic() {
        let link = link_with(Desk { breaking: true, waiting: true, ..Desk::default() });
        for raw in ["", "\r\n\r\n", "GET", "PUT /unlock?token=s3cret HTTP/1.1\r\n\r\n", "%%%"] {
            let reply = answer(raw.as_bytes(), "s3cret", &link, "test", None);
            assert!(reply.starts_with("HTTP/1.1 4"), "{raw:?} → {reply}");
        }
        assert!(!link.take_scan());
    }

    #[test]
    fn a_token_with_awkward_characters_survives_the_query_string() {
        let link = link_with(Desk { breaking: true, waiting: true, ..Desk::default() });
        let raw = "GET /unlock?token=a%20b%2Bc HTTP/1.1\r\n\r\n";
        assert!(answer(raw.as_bytes(), "a b+c", &link, "test", None).starts_with("HTTP/1.1 200"));
    }

    #[test]
    fn the_tag_url_follows_whatever_the_front_door_is() {
        let mut cfg = Config { listen: "127.0.0.1:9797".into(), token: "t0ken".into(), ..Config::default() };
        assert_eq!(cfg.tag_url(), "http://127.0.0.1:9797/unlock?token=t0ken");
        assert!(!cfg.fronted());

        // Behind a proxy the address tea listens on is not one the tag can
        // reach, so what gets printed has to be the front door instead.
        cfg.url = "https://tea.example".into();
        assert_eq!(cfg.tag_url(), "https://tea.example/unlock?token=t0ken");
        assert!(cfg.fronted());

        // Written either way round, it means the same thing.
        for written in ["https://tea.example/unlock", "https://tea.example/unlock/", "https://tea.example/"] {
            cfg.url = written.into();
            assert_eq!(cfg.tag_url(), "https://tea.example/unlock?token=t0ken", "{written}");
        }
    }

    #[test]
    fn a_proxy_gets_to_say_who_it_is_carrying() {
        // Every request through a proxy comes from the proxy, and a log full
        // of "from 127.0.0.1" is a log that answers nothing.
        assert_eq!(caller(Some("192.168.2.31"), "127.0.0.1"), "192.168.2.31 (via 127.0.0.1)");
        assert_eq!(caller(Some("192.168.2.31, 10.0.0.2"), "127.0.0.1"), "192.168.2.31 (via 127.0.0.1)");
        assert_eq!(caller(None, "192.168.2.31"), "192.168.2.31");
        assert_eq!(caller(Some("  "), "127.0.0.1"), "127.0.0.1");
    }

    fn asking(link: &Rc<Link>) -> Ask {
        Ask {
            host: "h".into(),
            port: 8123,
            tls: false,
            path: "/api/states/tag.hall".into(),
            token: "t".into(),
            entity: "tag.hall".into(),
            jobs: None,
            legs: None,
            gait: None,
            nudge: None,
            link: Rc::clone(link),
            baseline: RefCell::new(None),
            busy: Cell::new(false),
            complained: Cell::new(false),
            misses: Cell::new(0),
            lost: Cell::new(false),
        }
    }

    /// The same, with a walk to be counted alongside the tag.
    fn asking_with_legs(link: &Rc<Link>, needed: u32) -> Ask {
        Ask {
            legs: Some(Legs {
                misses: Cell::new(0),
                lost: Cell::new(false),
                entity: "sensor.steps".into(),
                path: "/api/states/sensor.steps".into(),
                needed,
                walker: RefCell::new(Walker::default()),
                complained: Cell::new(false),
            }),
            ..asking(link)
        }
    }

    /// And with time on your feet to be counted, at a two-second beat.
    fn asking_with_gait(link: &Rc<Link>, needed: u32) -> Ask {
        Ask {
            gait: Some(Gait {
                entity: "sensor.activity".into(),
                path: "/api/states/sensor.activity".into(),
                needed,
                states: Moving::default().states,
                beat: 2.0,
                secs: Cell::new(0.0),
                complained: Cell::new(false),
                misses: Cell::new(0),
                lost: Cell::new(false),
            }),
            ..asking(link)
        }
    }

    #[test]
    fn moving_words_add_up_and_still_ones_take_nothing_away() {
        let link = link_with(Desk { breaking: true, ..Desk::default() });
        let ask = asking_with_gait(&link, 6);
        let gait = ask.gait.as_ref().unwrap();

        settle_motion(&ask, gait, Ok("still".into()));
        assert_eq!(gait.motion(), Motion { secs: 0, needed: 6, lost: false });
        settle_motion(&ask, gait, Ok("walking".into()));
        settle_motion(&ask, gait, Ok("Walking".into()));
        assert_eq!(link.motion(), Some(Motion { secs: 4, needed: 6, lost: false }));
        // A still second between two walking ones is not a step backwards.
        settle_motion(&ask, gait, Ok("still".into()));
        settle_motion(&ask, gait, Ok("unknown".into()));
        assert_eq!(gait.motion().secs, 4);
        settle_motion(&ask, gait, Ok("on_foot".into()));
        assert!(link.motion().unwrap().done());
        assert_eq!(link.reachable(), Some(true));
    }

    #[test]
    fn an_activity_sensor_that_cannot_be_read_says_so_after_a_run_of_misses() {
        let link = link_with(Desk { breaking: true, ..Desk::default() });
        let ask = asking_with_gait(&link, 30);
        let gait = ask.gait.as_ref().unwrap();
        settle_motion(&ask, gait, Ok("walking".into()));
        for _ in 0..MISSES_BEFORE_LOST - 1 {
            settle_motion(&ask, gait, Err("no".into()));
            assert!(!gait.motion().lost, "one bad answer is a packet, not an outage");
        }
        settle_motion(&ask, gait, Err("no".into()));
        assert!(link.motion().unwrap().lost);
        assert_eq!(link.reachable(), Some(false));
        // Time already counted survives the outage, and an answer clears it.
        settle_motion(&ask, gait, Ok("still".into()));
        assert_eq!(link.motion(), Some(Motion { secs: 2, needed: 30, lost: false }));
        assert_eq!(link.reachable(), Some(true));
    }

    #[test]
    fn moving_is_configured_like_the_steps() {
        let mut cfg = Config::default();
        assert!(!cfg.counts_moving());
        cfg.mode = Mode::On;
        cfg.home_assistant.url = "http://h:8123".into();
        cfg.home_assistant.entity = "tag.hall".into();
        cfg.moving.mode = Mode::On;
        assert!(cfg.moving_misconfigured().unwrap().contains("entity"));
        cfg.moving.entity = "sensor.phone_detected_activity".into();
        assert!(cfg.counts_moving());
        // Auto, with no walk to scale from: thirty seconds.
        assert_eq!(cfg.moving_secs(), 30);
        // With a walk, a share of what the walk itself takes -- and never so
        // little that a lazy sensor cannot report it.
        cfg.steps.mode = Mode::On;
        cfg.steps.entity = "sensor.steps".into();
        cfg.steps.count = 100;
        assert_eq!(cfg.moving_secs(), 30);
        cfg.steps.count = 10;
        assert_eq!(cfg.moving_secs(), 5);
        cfg.steps.count = 1000;
        assert_eq!(cfg.moving_secs(), 120);
        assert!(cfg.moving_words().contains("auto"));
        // Said outright, it is what it says.
        cfg.moving.r#for = Wanted::For(Duration::from_secs(45));
        assert_eq!(cfg.moving_secs(), 45);
        assert_eq!(cfg.moving_words(), "45s");
        let parsed: Moving = toml::from_str(r#"for = "auto""#).unwrap();
        assert_eq!(parsed.r#for, Wanted::Auto);
        let parsed: Moving = toml::from_str(r#"for = "20s""#).unwrap();
        assert_eq!(parsed.r#for, Wanted::For(Duration::from_secs(20)));
        assert!(toml::from_str::<Moving>(r#"for = "sometimes""#).is_err());
        assert!(cfg.moving.counts(" WALKING "));
        assert!(!cfg.moving.counts("in_vehicle"));
        cfg.moving.states = vec![" ".into()];
        assert!(cfg.moving_misconfigured().unwrap().contains("states"));
    }

    /// A phone with nothing else in it, set up the way `watch` sets one up.
    fn poking(ha: &HomeAssistant) -> Nudge {
        let first = ha.nudge_first_beats();
        Nudge {
            service: "notify.mobile_app_pixel".into(),
            path: "/api/services/notify/mobile_app_pixel".into(),
            every: ha.nudge_beats(),
            first,
            due: Cell::new(first.saturating_sub(1)),
            busy: Cell::new(false),
            said: Cell::new(false),
            complained: Cell::new(false),
        }
    }

    #[test]
    fn the_phone_is_poked_ten_seconds_into_a_break_and_every_thirty_after_that() {
        let ha = HomeAssistant {
            url: "http://h:8123".into(),
            nudge: "notify.mobile_app_pixel".into(),
            ..Default::default()
        };
        // Five of the default two-second beats to the first poke, on the fifth
        // of them rather than the first: the first ten seconds of a break are
        // spent standing up, and a phone asked about a walk that has not
        // started yet has nothing to say. Fifteen to every round after that:
        // the count it reports is written a minute or two apart, and a pocket
        // buzzed every ten seconds about the same number is a pocket that
        // switches the whole thing off.
        assert_eq!(ha.nudge_every(), Duration::from_secs(30));
        let nudge = poking(&ha);
        let beats = |n: u32| (0..n).map(|_| nudge.pokes_now()).collect::<Vec<_>>();
        let opening = vec![false, false, false, false, true];
        let round = [vec![false; 14], vec![true]].concat();
        assert_eq!(beats(20), [opening.clone(), round.clone()].concat());
        // A break that ends mid-round lends none of its beats to the next one:
        // that break waits its own ten seconds, not the rest of a half minute.
        nudge.pokes_now();
        nudge.forget();
        assert_eq!(beats(5), opening);
        // And a beat that arrives while the last poke is still in flight is
        // skipped rather than queued -- which is the poll's own bargain, and
        // the reason `busy` is looked at before the beat is spent.
        nudge.busy.set(true);
        assert_eq!(nudge.due.get(), nudge.every.saturating_sub(1));
    }

    #[test]
    fn the_phone_is_left_alone_once_the_gate_has_what_it_wants() {
        let link = link_with(Desk { breaking: true, ..Default::default() });
        let ask = asking_with_legs(&link, 20);
        // Nothing walked yet, and the phone is the only thing that can say
        // otherwise.
        link.post_walk(Walk { walked: 0, needed: 20, marked: true });
        assert!(wanted(&ask));
        // The walk is in with minutes of the break still to run: a fresh
        // reading now would change nothing, so the phone is left in peace.
        link.post_walk(Walk { walked: 20, needed: 20, marked: true });
        assert!(!wanted(&ask));
        // Until a count that goes down moves the mark and starts the walk
        // over, which starts the poking over with it.
        link.post_walk(Walk { walked: 3, needed: 20, marked: true });
        assert!(wanted(&ask));
        // And a gate with neither half in it never wanted the phone at all.
        assert!(!wanted(&asking(&link)));
    }

    #[test]
    fn a_poll_slower_than_the_nudge_pokes_on_every_beat_of_it() {
        // The poll is the only beat there is, so the half minute is however
        // many polls land nearest to it -- and never fewer than one.
        let polling = |secs| HomeAssistant {
            poll: crate::config::Dur(Duration::from_secs(secs)),
            ..Default::default()
        };
        assert_eq!(polling(30).nudge_beats(), 1);
        assert_eq!(polling(30).nudge_every(), Duration::from_secs(30));
        assert_eq!(polling(30).nudge_first_beats(), 1);
        assert_eq!(polling(7).nudge_beats(), 4);
        assert_eq!(polling(7).nudge_every(), Duration::from_secs(28));
        assert_eq!(polling(7).nudge_first_beats(), 1);
        assert_eq!(polling(3).nudge_first_beats(), 3);
    }

    #[test]
    fn the_nudge_names_a_phone_and_says_when_there_is_no_point_sending_one() {
        let mut cfg = Config { mode: Mode::On, ..Default::default() };
        cfg.home_assistant.url = "http://h:8123".into();
        cfg.home_assistant.entity = "tag.hall".into();
        assert!(!cfg.home_assistant.nudges());
        assert!(cfg.nudge_misconfigured().is_none(), "nobody named, nothing to say");
        // The bare name is the one people have in front of them; `notify.` is
        // not a thing anybody would think to type.
        cfg.home_assistant.nudge = "mobile_app_pixel".into();
        assert!(cfg.home_assistant.nudges());
        assert_eq!(cfg.home_assistant.nudge_call(), Some(("notify", "mobile_app_pixel")));
        // On, with no sensor of the phone's being read: a notification every
        // half minute for nobody.
        assert!(cfg.nudge_misconfigured().unwrap().contains("both off"));
        cfg.steps.mode = Mode::On;
        cfg.steps.entity = "sensor.steps".into();
        assert!(cfg.nudge_misconfigured().is_none());
        // Written out in full it is taken as written.
        cfg.home_assistant.nudge = "notify.mobile_app_pixel".into();
        assert_eq!(cfg.home_assistant.nudge_call(), Some(("notify", "mobile_app_pixel")));
        assert!(cfg.nudge_misconfigured().is_none());
        // Anything that is not a service id would be a 404 every ten seconds
        // with nothing to say why, so it is said now instead.
        for wrong in ["notify.Mobile App", "notify/mobile_app_pixel", "notify.a.b"] {
            cfg.home_assistant.nudge = wrong.into();
            assert!(cfg.nudge_misconfigured().unwrap().contains("notify.mobile_app_pixel"), "{wrong}");
        }
        // And a hub to send it through is not optional: the poke goes to the
        // phone the long way round, through Home Assistant.
        cfg.home_assistant.nudge = "notify.mobile_app_pixel".into();
        cfg.home_assistant.url = String::new();
        assert!(cfg.nudge_misconfigured().unwrap().contains("url"));
    }

    #[test]
    fn a_poke_that_lands_says_nothing_and_one_that_does_not_says_which_phone() {
        let reply = |status: &str| format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\r\n[]");
        let poke = |status: &str| read_poke(reply(status).as_bytes(), "notify.mobile_app_pixel");
        // There is nothing in the reply to read: the phone's answer arrives as
        // a step count on a later poll, or does not arrive at all.
        assert!(poke("200 OK").is_ok());
        assert!(poke("201 Created").is_ok());
        // A phone the hub has never met -- never set up, renamed, logged out --
        // which is the mistake this setting invites.
        assert!(poke("404 Not Found").unwrap_err().contains("notify.mobile_app_pixel"));
        assert!(poke("400 Bad Request").unwrap_err().contains("companion app"));
        assert!(poke("401 Unauthorized").unwrap_err().contains("token"));
        assert!(poke("503 Service Unavailable").unwrap_err().contains("503"));
        assert!(read_poke(b"not http at all", "notify.mobile_app_pixel").is_err());
    }

    #[test]
    fn a_phone_that_will_not_be_told_costs_one_line_and_never_the_break() {
        let nudge = poking(&HomeAssistant::default());
        settle_poke(&nudge, Err("no route to host".into()));
        assert!(nudge.complained.get());
        settle_poke(&nudge, Err("no route to host".into()));
        assert!(nudge.complained.get(), "once an outage, not once a beat");
        settle_poke(&nudge, Ok(()));
        assert!(!nudge.complained.get(), "and the recovery is worth a word");
        // A break that ends during an outage starts the next one quiet, the
        // way every other complaint here is reset.
        settle_poke(&nudge, Err("no route to host".into()));
        nudge.forget();
        assert!(!nudge.complained.get());
    }

    #[test]
    fn a_changed_entity_is_a_scan_and_the_first_look_never_is() {
        let link = link_with(Desk { breaking: true, ..Desk::default() });
        let ask = asking(&link);

        // Whatever it says when the break starts is the "not yet" value --
        // otherwise a tag touched at three would pay for the four o'clock break.
        settle_tag(&ask, Ok("2026-08-31T09:00:00+00:00".into()));
        assert!(!link.take_scan(), "the first look is a baseline, never a scan");
        assert_eq!(link.reachable(), Some(true));

        // Same value, over and over, while nobody goes anywhere.
        settle_tag(&ask, Ok("2026-08-31T09:00:00+00:00".into()));
        assert!(!link.take_scan());

        settle_tag(&ask, Ok("2026-08-31T09:26:31+00:00".into()));
        assert!(link.take_scan(), "it changed — somebody went");

        // Reported once. The new value is the new normal, not a scan repeated
        // every two seconds for the rest of the break.
        settle_tag(&ask, Ok("2026-08-31T09:26:31+00:00".into()));
        assert!(!link.take_scan());
    }

    #[test]
    fn a_hub_that_restarts_does_not_end_your_break() {
        let link = link_with(Desk { breaking: true, ..Desk::default() });
        let ask = asking(&link);
        settle_tag(&ask, Ok("2026-08-31T09:00:00+00:00".into()));
        assert!(!link.take_scan());

        // Home Assistant comes back up and hands out `unknown` again. That is a
        // change, and it is emphatically not somebody walking to the hall.
        for empty in ["unknown", "unavailable", ""] {
            settle_tag(&ask, Ok(empty.into()));
            assert!(!link.take_scan(), "{empty:?} is not a scan");
        }

        // ...and a real scan after that still counts.
        settle_tag(&ask, Ok("2026-08-31T09:31:02+00:00".into()));
        assert!(link.take_scan());
    }

    #[test]
    fn an_entity_that_blinks_and_comes_back_unchanged_is_not_a_scan() {
        let link = link_with(Desk { breaking: true, ..Desk::default() });
        let ask = asking(&link);
        settle_tag(&ask, Ok("2026-08-31T09:00:00+00:00".into()));
        assert!(!link.take_scan());

        // A Zigbee blip, an integration reload: the entity vanishes for a few
        // polls and then comes back holding the very state it had before.
        // Nobody walked anywhere, and the page must not lift.
        settle_tag(&ask, Ok("unavailable".into()));
        assert!(!link.take_scan());
        settle_tag(&ask, Ok("2026-08-31T09:00:00+00:00".into()));
        assert!(!link.take_scan(), "recovering to the old state is not a scan");

        // A state it never had before is still somebody at the tag.
        settle_tag(&ask, Ok("2026-08-31T09:26:31+00:00".into()));
        assert!(link.take_scan());
    }

    #[test]
    fn a_break_that_starts_mid_outage_takes_the_recovery_as_its_baseline() {
        let link = link_with(Desk { breaking: true, ..Desk::default() });
        let ask = asking(&link);

        // The watcher is down when the break starts; whatever it recovers to
        // is old news, not a walk made during the outage.
        settle_tag(&ask, Ok("unavailable".into()));
        assert!(!link.take_scan());
        settle_tag(&ask, Ok("2026-08-31T09:00:00+00:00".into()));
        assert!(!link.take_scan());

        settle_tag(&ask, Ok("2026-08-31T09:26:31+00:00".into()));
        assert!(link.take_scan());
    }

    #[test]
    fn an_unanswered_question_is_reported_not_guessed_at() {
        let link = link_with(Desk { breaking: true, ..Desk::default() });
        let ask = asking(&link);
        let quiet = || Err::<String, String>("cannot reach 192.168.2.50:8123".to_string());

        // One unanswered question is a packet, not an outage. Acting on it
        // would end a break early every time a reply came back malformed --
        // and the gate that exists to make somebody walk would be opening
        // itself, quietly, a few times a week.
        for miss in 1..MISSES_BEFORE_LOST {
            settle_tag(&ask, quiet());
            assert_eq!(link.reachable(), None, "miss {miss} is not an outage yet");
        }
        settle_tag(&ask, quiet());
        assert_eq!(link.reachable(), Some(false), "a run of them is");
        assert!(!link.take_scan(), "silence is never a scan");

        // And one good answer is enough to be back: the run has to be
        // consecutive or a hub that drops one reply an hour is never trusted
        // again.
        settle_tag(&ask, Ok("2026-08-31T09:00:00+00:00".into()));
        assert_eq!(link.reachable(), Some(true));
        settle_tag(&ask, quiet());
        assert_eq!(link.reachable(), Some(true), "the count started again");
    }

    #[test]
    fn every_way_the_answer_goes_wrong_says_which_way() {
        let ok = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n\
                  {\"entity_id\":\"tag.hall\",\"state\":\"2026-08-31T09:26:31+00:00\",\
                  \"attributes\":{\"friendly_name\":\"Hall\"}}";
        assert_eq!(read_state(ok.as_bytes(), "tag.hall").unwrap(), "2026-08-31T09:26:31+00:00");

        for (reply, expected) in [
            ("HTTP/1.1 401 Unauthorized\r\n\r\n{}", "token"),
            ("HTTP/1.1 404 Not Found\r\n\r\n{}", "no entity called"),
            ("HTTP/1.1 500 Oops\r\n\r\n", "answered 500"),
            ("HTTP/1.1 200 OK\r\n\r\n<html>not json</html>", "cannot read the answer"),
            ("nonsense", "not HTTP"),
        ] {
            let err = read_state(reply.as_bytes(), "tag.hall").unwrap_err();
            assert!(err.contains(expected), "{reply:?} → {err}");
        }
    }

    #[test]
    fn a_token_can_live_in_a_file_shaped_however_it_arrives() {
        // Written the way a .env file is written...
        assert_eq!(
            read_env("# tea\nTEA_HA_TOKEN=eyJhbGci\n", TOKEN_VAR).as_deref(),
            Some("eyJhbGci")
        );
        // ...or the way a shell profile is...
        assert_eq!(
            read_env("export TEA_HA_TOKEN=\"eyJhbGci\"\n", TOKEN_VAR).as_deref(),
            Some("eyJhbGci")
        );
        assert_eq!(read_env("TEA_HA_TOKEN='eyJhbGci'", TOKEN_VAR).as_deref(), Some("eyJhbGci"));
        // ...or the way half of everyone will actually do it, given a file and
        // an instruction to put a token in it.
        assert_eq!(read_env("eyJhbGci\n", TOKEN_VAR).as_deref(), Some("eyJhbGci"));
        assert_eq!(read_env("# a comment\n\n  eyJhbGci  \n", TOKEN_VAR).as_deref(), Some("eyJhbGci"));

        // A file full of other things, with no token in it, is not a token.
        assert_eq!(read_env("OTHER=1\nSOMETHING=2\n", TOKEN_VAR), None);
        assert_eq!(read_env("", TOKEN_VAR), None);
        assert_eq!(read_env("# nothing but comments\n", TOKEN_VAR), None);
        // A JWT is full of dots and dashes and must survive intact.
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJpc3MiOiJ4In0.4dR8-puv5XbedyfwyVLPNB_EzeqhLZVaeo";
        assert_eq!(read_env(&format!("TEA_HA_TOKEN={jwt}"), TOKEN_VAR).as_deref(), Some(jwt));
    }

    #[test]
    fn the_token_is_looked_for_in_the_config_then_the_file_then_the_environment() {
        let mut ha = HomeAssistant { token: "  inline  ".into(), ..HomeAssistant::default() };
        assert_eq!(ha.secret().unwrap(), "inline");
        assert_eq!(ha.secret_source(), "written in this file");

        // Nothing anywhere is an error that names every place it looked, rather
        // than a break page that quietly never lifts.
        ha.token = String::new();
        let err = ha.secret().unwrap_err();
        for place in ["token", "token_file", TOKEN_VAR] {
            assert!(err.contains(place), "{err}");
        }

        ha.token_file = PathBuf::from("/does/not/exist.env");
        assert!(ha.secret().unwrap_err().contains("/does/not/exist.env"));
    }

    #[test]
    fn addresses_are_taken_apart_the_way_people_write_them() {
        assert_eq!(
            split_url("http://192.168.2.50:8123").unwrap(),
            ("192.168.2.50".into(), 8123, false, String::new())
        );
        // A trailing slash, and the default ports.
        assert_eq!(split_url("https://ha.example/").unwrap(), ("ha.example".into(), 443, true, String::new()));
        assert_eq!(split_url("http://ha.example").unwrap(), ("ha.example".into(), 80, false, String::new()));
        // Living under a path prefix behind somebody's proxy.
        assert_eq!(
            split_url("https://home.example/hass/").unwrap(),
            ("home.example".into(), 443, true, "/hass".into())
        );
        // A missing scheme is a mistake worth naming.
        assert!(split_url("192.168.2.50:8123").is_err());
        assert!(split_url("http://ha.example:hello").is_err());
        assert!(split_url("http://").is_err());
    }

    #[test]
    fn off_and_a_duration_both_read_as_a_grace() {
        #[derive(Deserialize)]
        struct Holder {
            grace: Grace,
        }
        let off: Holder = toml::from_str(r#"grace = "off""#).unwrap();
        assert_eq!(off.grace.0, Duration::ZERO);
        let ten: Holder = toml::from_str(r#"grace = "10m""#).unwrap();
        assert_eq!(ten.grace.0, Duration::from_secs(600));
        assert!(toml::from_str::<Holder>(r#"grace = "soon""#).is_err());
    }

    #[test]
    fn the_walk_is_counted_from_where_the_break_found_you() {
        let link = link_with(Desk { breaking: true, ..Desk::default() });
        let ask = asking_with_legs(&link, 20);
        let steps = |value: &str| settle(&ask, Ok("9:00".into()), Some(Ok(value.into())), None, None);

        // The daily total when the page went up: no walk yet, but the page has
        // to know a walk is being asked for.
        steps("4812.0");
        assert_eq!(link.walk(), Some(Walk { walked: 0, needed: 20, marked: false }));

        // The first sync of the break is the phone catching up. Whatever it
        // brings was walked before the page went up, so it moves the mark
        // instead of paying for the break.
        steps("4824.0");
        // And the page is told so, rather than left saying *0 of 20* to
        // somebody who has just walked across the flat.
        assert_eq!(link.walk(), Some(Walk { walked: 0, needed: 20, marked: true }));

        steps("4836.0");
        assert_eq!(link.walk(), Some(Walk { walked: 12, needed: 20, marked: true }));
        assert!(!link.walk().unwrap().done());

        steps("4844.0");
        assert_eq!(link.walk(), Some(Walk { walked: 20, needed: 20, marked: true }));
        assert!(link.walk().unwrap().done());
    }

    #[test]
    fn a_step_sensor_with_nothing_to_say_is_not_zero_steps() {
        let link = link_with(Desk { breaking: true, ..Desk::default() });
        let ask = asking_with_legs(&link, 20);
        settle(&ask, Ok("9:00".into()), Some(Ok("4812.0".into())), None, None);
        // The catch-up sync, and then a walk that actually counts.
        settle(&ask, Ok("9:00".into()), Some(Ok("4820.0".into())), None, None);
        settle(&ask, Ok("9:00".into()), Some(Ok("4840.0".into())), None, None);
        assert!(link.walk().unwrap().done());

        // A phone that has not synced, an integration reloading: the walk
        // already counted stands, and the hub is still perfectly reachable.
        for quiet in ["unknown", "unavailable", ""] {
            settle(&ask, Ok("9:00".into()), Some(Ok(quiet.into())), None, None);
            assert!(link.walk().unwrap().done(), "{quiet:?} must not undo the walk");
        }
        assert_eq!(link.reachable(), Some(true));
    }

    #[test]
    fn a_step_sensor_that_cannot_be_read_hands_the_desk_back() {
        // The half of the gate nobody can walk to. Unlike the tag, there is no
        // trying again in the hall: if the sensor cannot be read the page has
        // to come down on the clock, which is what `reachable` tells the engine.
        let link = link_with(Desk { breaking: true, ..Desk::default() });
        let ask = asking_with_legs(&link, 20);
        let gone = || Some(Err::<String, String>("no entity called that".to_string()));

        // Same debounce as the tag, and it has to be the *steps* that decide
        // it: the tag is answering perfectly well throughout.
        for _ in 1..MISSES_BEFORE_LOST {
            settle(&ask, Ok("9:00".into()), gone(), None, None);
            assert_ne!(link.reachable(), Some(false), "one miss is not an outage");
        }
        settle(&ask, Ok("9:00".into()), gone(), None, None);
        assert_eq!(link.reachable(), Some(false));

        // A tag that keeps answering must not paint over a walk that can never
        // be counted -- the gate still has a half that will not close.
        settle_tag(&ask, Ok("9:01".into()));
        assert_eq!(link.reachable(), Some(false), "the steps are still gone");
        settle(&ask, Ok("9:01".into()), Some(Ok("4812.0".into())), None, None);
        assert_eq!(link.reachable(), Some(true), "and back when both answer");
    }

    #[test]
    fn the_first_reading_of_a_break_is_a_yardstick_not_a_walk() {
        // Yesterday's ten thousand steps must not pay for this afternoon.
        let mut w = Walker::default();
        assert!(!w.saw(9_412.0), "the first reading is only the mark");
        assert_eq!(w.walked(), 0);
        // Nor is the first rise: see below.
        assert!(w.saw(9_432.0));
        assert_eq!(w.walked(), 0);
        assert!(!w.saw(9_452.0));
        assert_eq!(w.walked(), 20);
    }

    #[test]
    fn the_batch_a_phone_syncs_mid_break_is_not_a_walk() {
        // The property the whole step gate rests on. A daily total reports
        // steps when the phone syncs, not when they were walked: sit down at
        // 10:00 having walked all morning, and the first sync of the break can
        // arrive carrying two thousand of them. Credited, that opens the gate
        // from the chair -- which is the exact hole the steps were added to
        // close, so it must stay shut.
        let mut w = Walker::default();
        w.saw(4_000.0);
        assert!(w.saw(6_000.0), "the morning's walking is the phone catching up");
        assert_eq!(w.walked(), 0, "two thousand steps from a chair are not a walk");

        // And from there the gate works normally: this is measured from a total
        // the phone reported while the page was up.
        w.saw(6_020.0);
        assert_eq!(w.walked(), 20);
    }

    #[test]
    fn a_live_sensor_is_credited_its_first_rise() {
        // The phone's own step counter, reported every minute or so: the
        // reading a break starts from is a minute old at most, and a minute
        // ago you were in the chair. The first rise is the walk, and taking
        // it as the mark would have it walked twice.
        let mut w = Walker::new(Sync::Live);
        assert!(!w.saw(24_168.0), "the first reading is still only the mark");
        assert_eq!(w.walked(), 0);
        assert!(!w.saw(24_188.0), "and the first rise is not a yardstick");
        assert_eq!(w.walked(), 20, "it is the walk");
        assert!(!w.marked(), "so the page has no re-basing to explain");

        // A batched feed, for contrast, still moves the mark first.
        let mut w = Walker::new(Sync::Batched);
        w.saw(24_168.0);
        assert!(w.saw(24_188.0));
        assert_eq!(w.walked(), 0);
        assert!(w.marked());
    }

    #[test]
    fn a_live_sensor_that_counts_from_boot_still_survives_the_reboot() {
        // A since-reboot total is what the phone's own counter reports. The
        // number itself is meaningless -- it is measured from, never credited
        // -- and a phone restarted mid-break drops to single digits without
        // undoing the walk or paying for one.
        let mut w = Walker::new(Sync::Live);
        w.saw(24_168.0);
        w.saw(24_180.0);
        assert_eq!(w.walked(), 12);
        w.saw(3.0);
        assert_eq!(w.walked(), 12, "the reboot credits nothing and loses nothing");
        w.saw(11.0);
        assert_eq!(w.walked(), 20, "and counting resumes from the new total");

        // Between breaks the walk is forgotten; the kind of sensor is not.
        w.forget();
        assert_eq!(w.walked(), 0);
        w.saw(100.0);
        w.saw(105.0);
        assert_eq!(w.walked(), 5, "still live after a break");
    }

    #[test]
    fn sync_reads_the_words_people_actually_write() {
        #[derive(Deserialize)]
        struct S {
            sync: Sync,
        }
        let read = |word: &str| toml::from_str::<S>(&format!("sync = \"{word}\"")).map(|s| s.sync);
        for word in ["batched", "lagging", "delayed", "slow"] {
            assert_eq!(read(word).unwrap(), Sync::Batched, "{word}");
        }
        for word in ["live", "fast", "realtime", "prompt"] {
            assert_eq!(read(word).unwrap(), Sync::Live, "{word}");
        }
        assert!(read("instant").is_err(), "a word it does not know is an error, not a guess");
        assert_eq!(Steps::default().sync, Sync::Batched, "batched unless the file says otherwise");
    }

    #[test]
    fn a_total_that_corrects_itself_downwards_is_not_a_walk() {
        // A duplicate source dropped, a sync reconciled: the daily total steps
        // back a little. Read as a fresh counter it would credit the whole of
        // itself and open the gate from the chair.
        let mut w = Walker::default();
        w.saw(4_812.0);
        w.saw(4_800.0);
        assert_eq!(w.walked(), 0, "a correction is not four thousand steps");
        // And the walk carries on from the corrected total.
        w.saw(4_820.0);
        assert_eq!(w.walked(), 20);
    }

    #[test]
    fn a_counter_that_rolls_over_keeps_the_walk_and_carries_on_from_zero() {
        // Midnight, or a phone that re-pairs: the daily total starts again.
        // The walk so far stands, the new total is the new mark, and only one
        // poll's worth of steps falls down the gap between them.
        let mut w = Walker::default();
        w.saw(9_990.0);
        // The catch-up sync, then ten steps that count.
        w.saw(10_000.0);
        w.saw(10_010.0);
        assert_eq!(w.walked(), 10);
        w.saw(4.0);
        assert_eq!(w.walked(), 10, "the walk survives the reset");
        w.saw(9.0);
        assert_eq!(w.walked(), 15, "and counting resumes from the new total");
    }

    #[test]
    fn nothing_a_sensor_says_backwards_is_ever_credited() {
        // The property the feature rests on: only ground the sensor says was
        // covered is counted, and a total that drops is never itself a walk.
        let mut w = Walker::default();
        // 5000 sets the mark; 4999 and 12 both drop, and both credit nothing.
        for value in [5_000.0, 4_999.0, 12.0] {
            w.saw(value);
        }
        assert_eq!(w.walked(), 0, "two drops are not five thousand steps");

        // From 12 the total climbs 8, drops to 3, then climbs 5.
        for value in [20.0, 3.0, 8.0] {
            w.saw(value);
        }
        assert_eq!(w.walked(), 8 + 5);
    }

    #[test]
    fn a_sensor_that_repeats_itself_adds_nothing() {
        let mut w = Walker::default();
        for _ in 0..10 {
            w.saw(120.0);
        }
        assert_eq!(w.walked(), 0);
    }

    #[test]
    fn steps_are_only_counted_when_there_is_somewhere_to_count_them_from() {
        // On, but with no hub to ask and no sensor named: the gate must not
        // grow a half that nothing on earth could close.
        let mut cfg = Config { mode: Mode::On, ..Config::default() };
        cfg.steps.mode = Mode::On;
        cfg.steps.entity = "sensor.steps".into();
        assert!(!cfg.counts_steps(), "no hub, no steps");
        assert!(cfg.steps_misconfigured().is_some());

        cfg.home_assistant.url = "http://ha.example".into();
        cfg.home_assistant.entity = "tag.hall".into();
        assert!(cfg.counts_steps());
        assert_eq!(cfg.steps.count, 20, "twenty unless the file says otherwise");
        assert!(cfg.steps_misconfigured().is_none());

        cfg.steps.entity = String::new();
        assert!(!cfg.counts_steps());
        assert!(cfg.steps_misconfigured().is_some(), "on with nothing to read is worth saying");

        // And with the tag itself off, steps are not a gate of their own.
        cfg.steps.entity = "sensor.steps".into();
        cfg.mode = Mode::Off;
        assert!(!cfg.counts_steps());
        assert!(cfg.steps_misconfigured().is_none());
    }

    #[test]
    fn the_walk_reads_the_words_people_actually_write() {
        let cfg: Config =
            toml::from_str("mode = \"on\"\n[steps]\nmode = \"active\"\ncount = 40\nentity = \"sensor.s\"\n")
                .unwrap();
        assert_eq!(cfg.steps.mode, Mode::On);
        assert_eq!(cfg.steps.count, 40);

        let off: Config = toml::from_str("mode = \"on\"\n[steps]\nmode = \"inactive\"\n").unwrap();
        assert_eq!(off.steps.mode, Mode::Off);
        assert_eq!(off.steps.count, 20, "the default survives a table that only says off");
    }

    #[test]
    fn the_switch_answers_to_the_words_people_actually_write() {
        #[derive(Deserialize)]
        struct Holder {
            mode: Mode,
        }
        for word in ["on", "enabled", "active"] {
            let h: Holder = toml::from_str(&format!("mode = \"{word}\"")).unwrap();
            assert_eq!(h.mode, Mode::On, "{word}");
        }
        for word in ["off", "disabled", "inactive"] {
            let h: Holder = toml::from_str(&format!("mode = \"{word}\"")).unwrap();
            assert_eq!(h.mode, Mode::Off, "{word}");
        }
        assert!(toml::from_str::<Holder>("mode = \"maybe\"").is_err());
    }
}

/// The list in the corner: what gets read off the hub, what gets shown, and
/// what counts as a job done during a break.
#[cfg(test)]
mod chore_tests {
    use super::*;

    /// A reply in exactly the shape a hub sends one.
    fn reply(items: &str) -> Vec<u8> {
        format!(
            "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n\
             {{\"changed_states\":[],\"service_response\":{{\"todo.jobs\":{{\"items\":[{items}]}}}}}}"
        )
        .into_bytes()
    }

    fn open(uid: &str, summary: &str) -> Job {
        Job { uid: uid.into(), summary: summary.into(), done: false, completed: String::new(), due: String::new() }
    }

    fn done(uid: &str, summary: &str) -> Job {
        finished(uid, summary, "2026-09-02T09:00:00+00:00")
    }

    fn finished(uid: &str, summary: &str, at: &str) -> Job {
        Job { uid: uid.into(), summary: summary.into(), done: true, completed: at.into(), due: String::new() }
    }

    /// The day every test lives on: the one the stamps below are written in.
    const DAY: &str = "2026-09-02";

    impl Jobs {
        fn settle_on_day(&self, items: Vec<Job>) -> Board {
            self.settle_on(items, DAY)
        }
        fn lay_out_day(&self, items: &[Job]) -> Board {
            self.lay_out(items, DAY)
        }
    }

    fn list(cap: usize) -> Jobs {
        Jobs {
            entity: "todo.jobs".into(),
            title: "todo.jobs".into(),
            path: "/api/services/todo/get_items?return_response=true".into(),
            body: json_body("todo.jobs"),
            cap,
            every: 5,
            due: Cell::new(0),
            board: RefCell::new(None),
            open: RefCell::new(HashSet::new()),
            credited: RefCell::new(HashSet::new()),
            complained: Cell::new(false),
        }
    }

    #[test]
    fn a_reply_becomes_lines() {
        let raw = reply(
            "{\"summary\":\"Luft\",\"uid\":\"a\",\"status\":\"needs_action\"},\
             {\"summary\":\"Clean windows\",\"uid\":\"b\",\"status\":\"completed\",\
              \"completed\":\"2026-09-02T15:06:15+00:00\"}",
        );
        let jobs = read_jobs(&raw, "todo.jobs").unwrap();
        assert_eq!(
            jobs,
            vec![open("a", "Luft"), finished("b", "Clean windows", "2026-09-02T15:06:15+00:00")]
        );
    }

    #[test]
    fn a_line_with_no_words_on_it_is_not_a_line() {
        // A list somebody has just pressed "add" on, and not typed into yet.
        let raw = reply("{\"summary\":\"  \",\"uid\":\"a\",\"status\":\"needs_action\"}");
        assert!(read_jobs(&raw, "todo.jobs").unwrap().is_empty());
    }

    #[test]
    fn a_hub_that_says_no_says_why() {
        let refused = b"HTTP/1.0 401 Unauthorized\r\n\r\n{}".to_vec();
        assert!(read_jobs(&refused, "todo.jobs").unwrap_err().contains("token"));
        // A service call about an entity that is not a list, which is the
        // typo everybody makes: `sensor.` where `todo.` was meant.
        let wrong = b"HTTP/1.0 400 Bad Request\r\n\r\n{}".to_vec();
        assert!(read_jobs(&wrong, "sensor.oops").unwrap_err().contains("nfc.chores.entity"));
        // And a hub with no to-do integration at all.
        let missing = b"HTTP/1.0 404 Not Found\r\n\r\n{}".to_vec();
        assert!(read_jobs(&missing, "todo.jobs").unwrap_err().contains("To-do list integration"));
    }

    fn shown(board: &Board) -> Vec<&str> {
        board.jobs.iter().map(|j| j.summary.as_str()).collect()
    }

    #[test]
    fn the_page_shows_the_open_ones_first_and_no_more_than_it_can_hold() {
        let jobs = list(4);
        let board = jobs.settle_on_day(vec![
            finished("a", "Make tea", "2026-09-02T09:00:00+00:00"),
            open("b", "Luft"),
            finished("c", "Clean windows", "2026-09-02T11:00:00+00:00"),
            open("d", "Tidy up"),
        ]);
        // Open ones in the list's own order, then the finished ones, freshest
        // first -- the windows were done after the tea.
        assert_eq!(shown(&board), ["Luft", "Tidy up", "Clean windows", "Make tea"]);
    }

    #[test]
    fn a_long_list_still_finds_room_for_what_has_been_done() {
        // Six things to do and five lines: without a slot held back, the page
        // would be nothing but work, which is the opposite of the point.
        let jobs = list(5);
        let board = jobs.settle_on_day(vec![
            open("a", "Luft"),
            open("b", "Tidy up the Kitchen"),
            open("c", "Vacuum"),
            open("d", "Clean dish rack"),
            open("e", "Clean Mirros"),
            open("f", "Clean bathroom"),
            finished("g", "Make tea", "2026-09-02T15:15:29+00:00"),
        ]);
        assert_eq!(
            shown(&board),
            ["Luft", "Tidy up the Kitchen", "Vacuum", "Clean dish rack", "Make tea"]
        );
    }

    #[test]
    fn a_list_with_little_left_on_it_fills_up_with_what_was_done() {
        // The other way round: one job left, and the rest of the corner given
        // over to the afternoon's work rather than left blank.
        let jobs = list(4);
        let board = jobs.settle_on_day(vec![
            open("a", "Luft"),
            finished("b", "Bins", "2026-09-02T09:00:00+00:00"),
            finished("c", "Windows", "2026-09-02T10:00:00+00:00"),
            finished("d", "Dishes", "2026-09-02T11:00:00+00:00"),
            finished("e", "Tea", "2026-09-02T12:00:00+00:00"),
        ]);
        assert_eq!(shown(&board), ["Luft", "Tea", "Dishes", "Windows"]);
    }

    #[test]
    fn a_list_of_nothing_but_work_still_fills_the_page() {
        let jobs = list(3);
        let board = jobs.settle_on_day(vec![open("a", "Luft"), open("b", "Bins"), open("c", "Tea")]);
        assert_eq!(shown(&board), ["Luft", "Bins", "Tea"], "no finished jobs, no slots held");
    }

    #[test]
    fn the_page_can_say_what_the_list_is_called() {
        let mut chores = Chores { entity: "todo.physical_todos".into(), ..Chores::default() };
        assert_eq!(chores.header(), "todo.physical_todos", "the entity id unless told otherwise");
        chores.title = "  While you are up  ".into();
        assert_eq!(chores.header(), "While you are up");
    }

    #[test]
    fn what_is_ticked_off_drops_under_what_is_still_to_do() {
        let jobs = list(5);
        let board = jobs.settle_on_day(vec![open("a", "Luft"), open("b", "Tidy up"), open("c", "Mirrors")]);
        assert_eq!(shown(&board), ["Luft", "Tidy up", "Mirrors"]);
        // The middle one is done from the phone: it goes to the bottom, struck
        // through, and the ones still open close up above it.
        let board = jobs.lay_out_day(&[
            open("a", "Luft"),
            finished("b", "Tidy up", "2026-09-03T09:10:00+00:00"),
            open("c", "Mirrors"),
        ]);
        assert_eq!(
            board.jobs.iter().map(|j| (j.summary.as_str(), j.done)).collect::<Vec<_>>(),
            [("Luft", false), ("Mirrors", false), ("Tidy up", true)]
        );
    }

    #[test]
    fn a_freed_row_goes_to_the_job_that_did_not_fit() {
        // The page as it was on the morning this was written: five lines, four
        // open jobs, two done, so one open job is only a "1 more" marker.
        let jobs = list(5);
        let before = vec![
            open("a", "Luft"),
            open("b", "Tidy up the Kitchen"),
            open("c", "Clean Mirros"),
            open("d", "Clean bathroom"),
            finished("e", "Vacuum", "2026-09-02T15:00:00+00:00"),
            finished("f", "Clean dish rack", "2026-09-02T14:00:00+00:00"),
        ];
        let board = jobs.settle_on_day(before);
        assert_eq!(shown(&board), ["Luft", "Tidy up the Kitchen", "Clean Mirros", "Vacuum", "Clean dish rack"]);
        assert_eq!(board.hidden, 1);

        // The kitchen gets done. Its row goes to the bathroom, the marker goes
        // away, and the kitchen is the freshest of the finished ones.
        let after = vec![
            open("a", "Luft"),
            finished("b", "Tidy up the Kitchen", "2026-09-03T09:10:00+00:00"),
            open("c", "Clean Mirros"),
            open("d", "Clean bathroom"),
            finished("e", "Vacuum", "2026-09-02T15:00:00+00:00"),
            finished("f", "Clean dish rack", "2026-09-02T14:00:00+00:00"),
        ];
        let board = jobs.lay_out_day(&after);
        assert_eq!(shown(&board), ["Luft", "Clean Mirros", "Clean bathroom", "Tidy up the Kitchen", "Vacuum"]);
        assert_eq!(board.hidden, 0);
    }

    #[test]
    fn the_day_is_counted_off_the_list_not_off_the_page() {
        // Three lines of room. Three things done today by the hub's stamps,
        // one yesterday evening, and only two of them fit on the page -- the
        // count is still three, and it is three on whichever page shows it.
        let jobs = list(3);
        let board = jobs.settle_on_day(vec![
            open("a", "Clean Mirros"),
            finished("b", "Luft", "2026-09-02T12:21:05.222188+00:00"),
            finished("c", "Tidy up the Kitchen", "2026-09-02T12:10:22+00:00"),
            finished("d", "Vacuum", "2026-09-01T12:38:05+00:00"),
            finished("e", "Dishes", "2026-09-02T11:00:00+00:00"),
        ]);
        assert_eq!(shown(&board), ["Clean Mirros", "Luft", "Tidy up the Kitchen"]);
        assert_eq!(board.today, 3);
        // And one ticked off with no stamp at all is done, but not datably.
        let unstamped = Job { uid: "a".into(), summary: "Clean Mirros".into(), done: true, completed: String::new(), due: String::new() };
        let board = jobs.lay_out_day(&[unstamped]);
        assert_eq!(board.today, 0);
    }

    #[test]
    fn what_has_a_deadline_goes_to_the_top_soonest_first() {
        let jobs = list(5);
        let due = |uid: &str, summary: &str, when: &str| Job { due: when.into(), ..open(uid, summary) };
        let board = jobs.lay_out_day(&[
            open("a", "Clean toilet"),
            due("b", "Dentist", "2099-03-02"),
            open("c", "Luft"),
            due("d", "Pick Anni up", "2099-03-01T16:30:00+02:00"),
            finished("e", "Vacuum", "2026-09-02T09:00:00+00:00"),
        ]);
        assert_eq!(shown(&board), ["Pick Anni up", "Dentist", "Clean toilet", "Luft", "Vacuum"]);
        let chores = board.chores();
        assert_eq!(chores[0].due.as_ref().unwrap().label, "1 Mar");
        assert!(chores[2].due.is_none());
        assert!(chores[4].due.is_none(), "a finished job's deadline is nobody's business");
    }

    #[test]
    fn a_job_deleted_mid_break_goes() {
        let jobs = list(5);
        jobs.settle_on_day(vec![open("a", "Luft"), open("b", "Tidy up")]);
        let board = jobs.lay_out_day(&[open("b", "Tidy up")]);
        assert_eq!(shown(&board), ["Tidy up"]);
    }

    #[test]
    fn only_what_was_open_when_the_page_went_up_counts() {
        let jobs = list(5);
        let items = vec![open("a", "Luft"), open("b", "Tidy up"), done("c", "Make tea")];
        jobs.settle_on_day(items.clone());
        // Nothing has happened yet, and the job that was already done when the
        // break started is not this break's doing.
        assert_eq!(jobs.tally(&items), 0);

        let after = vec![done("a", "Luft"), open("b", "Tidy up"), done("c", "Make tea")];
        assert_eq!(jobs.tally(&after), 1);
        // Asked again on the next beat: still one job, not two.
        assert_eq!(jobs.tally(&after), 1);
    }

    #[test]
    fn a_job_ticked_off_and_put_back_was_still_done() {
        let jobs = list(5);
        let items = vec![open("a", "Luft")];
        jobs.settle_on_day(items.clone());
        assert_eq!(jobs.tally(&[done("a", "Luft")]), 1);
        assert_eq!(jobs.tally(&items), 1, "un-ticking is not un-doing");
    }

    #[test]
    fn what_did_not_fit_is_counted_and_recounted() {
        let jobs = list(3);
        let items = vec![
            open("a", "Luft"),
            open("b", "Tidy up"),
            open("c", "Vacuum"),
            open("d", "Bins"),
            finished("e", "Make tea", "2026-09-02T09:00:00+00:00"),
        ];
        let board = jobs.settle_on_day(items.clone());
        // Two open jobs on the page, one line kept for the finished one, and
        // two open jobs with nowhere to go -- which the page has to say.
        assert_eq!(shown(&board), ["Luft", "Tidy up", "Make tea"]);
        assert_eq!(board.hidden, 2);

        // One of the two nobody could see is ticked off from the phone. Two
        // finished now, so two lines are held for them and the page is down
        // to one open row -- but the marker says how many did not fit, and
        // that is still two, not the "1" a fixed page would have needed to
        // recount its way to.
        let after = vec![
            open("a", "Luft"),
            open("b", "Tidy up"),
            open("c", "Vacuum"),
            finished("d", "Bins", "2026-09-02T14:00:00+00:00"),
            finished("e", "Make tea", "2026-09-02T09:00:00+00:00"),
        ];
        let board = jobs.lay_out_day(&after);
        assert_eq!(shown(&board), ["Luft", "Bins", "Make tea"]);
        assert_eq!(board.hidden, 2);
        assert_eq!(jobs.tally(&after), 1, "and it counts, page or no page");
    }

    #[test]
    fn a_list_that_fits_hides_nothing() {
        let jobs = list(8);
        let board = jobs.settle_on_day(vec![open("a", "Luft"), finished("b", "Tea", "2026-09-02T09:00:00+00:00")]);
        assert_eq!(board.hidden, 0);
    }

    #[test]
    fn jobs_beyond_the_page_still_count() {
        // Two lines of room, four jobs. Ticking off the one that never fit on
        // screen is still a job done on a break.
        let jobs = list(2);
        let items =
            vec![open("a", "Luft"), open("b", "Tidy up"), open("c", "Vacuum"), open("d", "Bins")];
        jobs.settle_on_day(items.clone());
        let after = vec![open("a", "Luft"), open("b", "Tidy up"), open("c", "Vacuum"), done("d", "Bins")];
        assert_eq!(jobs.tally(&after), 1);
    }

    #[test]
    fn the_list_is_asked_about_on_its_own_beat() {
        let jobs = list(5);
        // The first beat of a break always asks; the next four do not.
        assert!(jobs.asks_now());
        assert_eq!((0..4).filter(|_| jobs.asks_now()).count(), 0);
        assert!(jobs.asks_now());
    }

    #[test]
    fn a_break_that_has_ended_forgets_the_list() {
        let jobs = list(5);
        jobs.settle_on_day(vec![open("a", "Luft")]);
        assert_eq!(jobs.tally(&[done("a", "Luft")]), 1);
        jobs.forget();
        assert!(jobs.board.borrow().is_none());
        // And the next break starts from nothing, not from yesterday's one.
        assert_eq!(jobs.tally(&[done("a", "Luft")]), 0);
    }

    #[test]
    fn an_entity_id_cannot_break_out_of_the_body() {
        assert_eq!(json_body("todo.jobs"), "{\"entity_id\":\"todo.jobs\"}");
        assert_eq!(json_body("a\"b"), "{\"entity_id\":\"a\\\"b\"}");
    }

    #[test]
    fn the_list_is_off_unless_there_is_a_hub_and_a_list() {
        let mut cfg = Config { mode: Mode::On, ..Config::default() };
        cfg.chores.mode = Mode::On;
        cfg.chores.entity = "todo.jobs".into();
        assert!(!cfg.shows_chores(), "no hub, no list");
        assert!(cfg.chores_misconfigured().is_some());

        cfg.home_assistant.url = "http://ha.example".into();
        cfg.home_assistant.entity = "tag.hall".into();
        assert!(cfg.shows_chores());
        assert!(cfg.chores_misconfigured().is_none());

        // The typo worth catching before it becomes an empty corner.
        cfg.chores.entity = "sensor.jobs".into();
        assert!(cfg.chores_misconfigured().unwrap().contains("to-do list"));

        // And unlike the steps, the list does not need the tag: it holds
        // nothing back, so there is nothing to be locked out of.
        cfg.chores.entity = "todo.jobs".into();
        cfg.mode = Mode::Off;
        assert!(cfg.shows_chores(), "a list is not a gate");
    }

    #[test]
    fn the_page_never_shows_more_than_it_can_hold() {
        let mut chores = Chores { show: 500, ..Chores::default() };
        assert_eq!(chores.cap(), CHORES_CAP as usize);
        chores.show = 0;
        assert_eq!(chores.cap(), 1, "clamped, not zero -- `show = 0` is off, not empty");
        chores.show = 8;
        assert_eq!(chores.cap(), 8, "eight is the default and well under the ceiling");
    }
}

/// The settings page, served off the same ear as the tag.
#[cfg(test)]
mod web_tests {
    use super::*;

    fn ask(raw: &str, site: Option<&crate::web::Site>) -> String {
        answer(raw.as_bytes(), "s3cret", &Link::new(), "test", site)
    }

    #[test]
    fn without_a_site_the_page_is_not_there() {
        let reply = ask("GET /settings?token=s3cret HTTP/1.1\r\n\r\n", None);
        assert!(reply.starts_with("HTTP/1.1 404"), "{reply}");
    }

    /// The dashboard rides on the settings page's switch, because it rides on
    /// the settings page's port. Wanting to look at a chart is not a reason to
    /// have opened one.
    #[test]
    fn without_a_site_the_dashboard_is_not_there_either() {
        let reply = ask("GET /dash?token=s3cret HTTP/1.1\r\n\r\n", None);
        assert!(reply.starts_with("HTTP/1.1 404"), "{reply}");
    }

    #[test]
    fn the_dashboard_is_served_behind_the_same_token() {
        let dir = std::env::temp_dir().join(format!("tea-dash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "work = \"30m\"\n").unwrap();
        let site = crate::web::Site { path: path.clone() };

        let page = ask("GET /dash?token=s3cret HTTP/1.1\r\n\r\n", Some(&site));
        assert!(page.starts_with("HTTP/1.1 200"), "{page}");
        assert!(page.contains("<title>tea — the last few weeks"), "the page itself");
        assert!(!page.contains("/*__TEA_DATA__*/"), "with the numbers actually in it");

        let wrong = ask("GET /dash?token=nope HTTP/1.1\r\n\r\n", Some(&site));
        assert!(wrong.starts_with("HTTP/1.1 401"), "{wrong}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What lets the settings page offer a setting your own file has never
    /// mentioned: the file as it ships, handed over beside the file you have.
    ///
    /// Renaming this door on one side only is the quiet kind of breakage --
    /// nothing errors, the page simply goes back to showing you the lines you
    /// already had -- so the page is asked here whether it still knocks on it.
    #[test]
    fn the_shipped_file_is_served_where_the_page_asks_for_it() {
        let dir = std::env::temp_dir().join(format!("tea-defaults-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "work = \"30m\"\n").unwrap();
        let site = crate::web::Site { path };

        let reply = ask("GET /defaults?token=s3cret HTTP/1.1\r\n\r\n", Some(&site));
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        assert!(reply.contains("[nfc.steps]"), "the shipped file itself comes back: {reply}");
        assert!(
            crate::web::PAGE.contains("'/defaults'"),
            "the settings page no longer asks for /defaults, so this door opens onto nobody"
        );

        let wrong = ask("GET /defaults?token=nope HTTP/1.1\r\n\r\n", Some(&site));
        assert!(wrong.starts_with("HTTP/1.1 401"), "{wrong}");

        // Off wherever the settings page is off. A catalogue of settings is not
        // a reason to answer anything on a port that was opened for a tag.
        let shut = ask("GET /defaults?token=s3cret HTTP/1.1\r\n\r\n", None);
        assert!(shut.starts_with("HTTP/1.1 404"), "{shut}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_request_is_complete_when_its_body_is() {
        assert!(!complete(b"POST /config HTTP/1.1\r\nContent-Length: 5\r\n"));
        assert!(!complete(b"POST /config HTTP/1.1\r\nContent-Length: 5\r\n\r\nab"));
        assert!(complete(b"POST /config HTTP/1.1\r\ncontent-length: 5\r\n\r\nabcde"));
        assert!(complete(b"GET /unlock HTTP/1.1\r\n\r\n"), "no body promised, none waited for");
    }

    #[test]
    fn the_file_goes_out_and_comes_back_behind_the_token() {
        let dir = std::env::temp_dir().join(format!("tea-ear-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "work = \"30m\"\n").unwrap();
        let site = crate::web::Site { path: path.clone() };

        let page = ask("GET /settings?token=s3cret HTTP/1.1\r\n\r\n", Some(&site));
        assert!(page.starts_with("HTTP/1.1 200") && page.contains("<title>tea settings"), "{page}");

        let read = ask("GET /config HTTP/1.1\r\nX-Tea-Token: s3cret\r\n\r\n", Some(&site));
        assert!(read.ends_with("\r\n\r\nwork = \"30m\"\n"), "{read}");

        let body = "work = \"25m\"\n";
        let post = format!(
            "POST /config HTTP/1.1\r\nX-Tea-Token: s3cret\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let saved = ask(&post, Some(&site));
        assert!(saved.starts_with("HTTP/1.1 200") && saved.contains("saved"), "{saved}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);

        // Short of what it promised: nothing written.
        let short = "POST /config HTTP/1.1\r\nX-Tea-Token: s3cret\r\nContent-Length: 40\r\n\r\nwork = \"1m\"\n";
        assert!(ask(short, Some(&site)).starts_with("HTTP/1.1 400"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);

        // Wrong token: not even a read.
        let wrong = ask("GET /config HTTP/1.1\r\nX-Tea-Token: nope\r\n\r\n", Some(&site));
        assert!(wrong.starts_with("HTTP/1.1 401"), "{wrong}");

        // A file that would not load is refused with the reason.
        let bad = "banana = 1\n";
        let post = format!("POST /config HTTP/1.1\r\nX-Tea-Token: s3cret\r\nContent-Length: {}\r\n\r\n{bad}", bad.len());
        let refused = ask(&post, Some(&site));
        assert!(refused.starts_with("HTTP/1.1 400") && refused.contains("not saved"), "{refused}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

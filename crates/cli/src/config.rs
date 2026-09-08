//! Config file loading. Defaults < config.toml < command-line flags.

use crate::clock::{self, Clock, Days, Now};
use serde::{Deserialize, de};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DEFAULT_FILE: &str = r##"# tea — take a break, whether you like it or not
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
window = "1h"         # the window that budget is counted over

# While an app holds the session awake -- a call, a video, a presentation --
# a break that falls due waits rather than covering the screen, and tea says
# so after a while.
#
#   warn_after  how long a break may be held up before tea mentions it and
#               names what is holding it. "0s" never mentions it.
#   ignore      inhibitors that do not count. An app id or reason containing
#               any of these strings is passed over as if it were not there,
#               so something of your own that keeps the screen on while a
#               long job runs does not read as a call. Case does not matter.
[calls]
warn_after = "20m"
ignore = []           # e.g. ["tmux agents"]

# When tea is awake at all. Outside these hours nothing is counted and nothing
# appears: an evening film is not a work session with the timer paused, it is
# not a work session. `tea off 1h` is the same idea for one afternoon.
#
#   from/to  local times, "09:00". "off" for no limit at either end. Setting
#            only one means the other end of the day. A window that runs
#            backwards -- from "22:00" to "06:00" -- wraps midnight, for
#            anybody who works those hours.
#   days     "all", "mon-fri", "mon,wed,fri", or any mixture. Ranges may wrap:
#            "fri-mon" is a long weekend.
[hours]
from = "off"
to = "off"
days = "all"

# Every so often, a longer break. Four five-minute breaks in a row are four
# chances to stand up and no chance to go anywhere; the long one is the walk,
# the coffee, the thing that does not fit in three hundred seconds.
#
#   every   every this-many-th break is the long one. 0 for none.
#   length  how long that one runs. Must be longer than `break`.
[long]
every = 0
length = "15m"

[hold]
# What the break page does when you switch away from it.
#   "soft"   — it covers the screen and leaves it at that. Alt-Tab, or the
#              Activities key, and you are back at your desk with the page
#              sitting behind everything.
#   "insist" — it puts itself back in front, again, for as long as the break
#              lasts. You can still get out, but only by keeping at it.
#   "strict" — insist, and the desktop's ways out go with it: the Super key,
#              the overview, Alt-Tab, the workspace switches, the dock's
#              number keys and the hot corner are switched off for the length
#              of the break and put back after -- at the next start too, if
#              tea was killed mid-break. The console and the power button
#              remain; the point is that leaving takes a decision. GNOME only.
mode = "soft"
recheck = "400ms"     # how often an insisting page checks it is still in front

# Noise, when a break starts and ends. Silent until you ask for it: a sound you
# did not choose, played at you several times an hour, is worse than no sound
# at all.
#
#   mode         "off" (default), "chime" for a short sound from the desktop's
#                own sound theme, "voice" to have it spoken through the
#                screen-reader voice, or "both".
#   start_file   a sound file played when the break starts. Empty takes the
#                desktop's theme rather than a file of your own.
#   end_file     the same, for the moment the break ends.
#   scan_file    what a successful scan sounds like. Empty plays the chime tea
#                ships, so the celebration works without anybody hunting the
#                internet for a sound file.
#   start_words  what "voice" says at the start.
#   end_words    and at the end.
#   scan_words   and on a scan. Empty says nothing.
[sound]
mode = "off"
start_file = ""
end_file = ""
scan_file = ""
start_words = "Time for a break"
end_words = "Break over"
scan_words = ""

# How the break page arrives. It does not simply appear -- it lands, because a
# page that fades in is a page you argue with.
#
#   entrance  how long the arrival takes. "0s" for none, and the page is
#             simply there.
#   burst     the share of that time the blast stays visible, 0 to 1. 0 is a
#             plain fade with nothing thrown outward.
#   shards    how many pieces of debris fly out of it.
[animation]
entrance = "3s"
burst = 0.62
shards = 26

# How the page looks, and what it says while the clock runs. The defaults are
# the page as it ships; nothing here has to be set.
#
#   accent      the colour of the ring, the glow, the blast and the pills, as
#               "#rrggbb". The text stays as it is: it is white on dark for a
#               reason, and an accent is a highlight, not a theme.
#   background  "dark" covers the screen. "dim" leaves your desktop showing
#               through the dark, so the page reads as a veil over your work
#               rather than a wall in front of it. Same page either way.
#   font        a family name, "IBM Plex Mono". Empty uses the first monospaced
#               face the machine has. Monospaced is the point: the clock and
#               the count change under you, and proportional digits twitch.
#   prompts     what the page says under "Time to stop" while the countdown
#               runs. "off" keeps the one line it has always said. "on" cycles
#               through a short built-in list -- look away, roll your
#               shoulders, drink some water -- and a list of your own does the
#               same with your words: prompts = ["Water.", "Look out of the window."]
#   prompt_every  how long each one stays up.
[page]
accent = "#7aa2ff"
background = "dark"
font = ""
prompts = "off"
prompt_every = "20s"

# The one port tea answers on. The tag's URL points here when tea does its own
# listening (see [nfc]), and the settings page is served here (see [settings]).
# Loopback by default, which answers this machine and nothing else; a phone in
# another room needs an address on your network, "0.0.0.0:9797". The token is
# the secret both need -- `tea settings` or `tea set-nfc on` writes one.
[port]
listen = "127.0.0.1:9797"
token = ""

[nfc]
# Sitting out a break at your own desk is not a break. With this on, the page
# does not lift when the countdown ends -- it lifts when a tag you have to get
# up and walk to says you went. Scanning early counts: the page still runs its
# full time, and then simply ends.
#
#   mode    "off" (default) or "on". "enabled"/"active" also read as on.
#   url     where the tag's URL really points, when something else is the front
#           door -- a reverse proxy on a box that is already listening, with
#           this machine dialling out to it. Nothing here listens to the
#           network in that arrangement. See "The ear" in the README. Where
#           tea itself answers, and the secret in the tag's URL, are [port].
#   grace   give up on the tag after this long and hand the desk back anyway,
#           so a flat phone does not cost you an afternoon. "off" waits.
#   prompt  what the page says while it waits. Yours knows where your tag is.
mode = "off"
url = ""
grace = "10m"
prompt = "Scan the tag to get your desk back"

# The other way round, and the tidier one: nothing reaches tea at all, tea does
# the asking. Home Assistant already knows when a tag is scanned -- its app
# fires the event -- so tea watches the entity and reads any change of its state
# as a scan. No open port, nothing forwarded in, and a hub that cannot be
# reached is something tea finds out about itself, in time to say so on the page
# rather than leaving you to discover it in another room.
#
# Only asked while a break is on screen. Set url and entity to switch it on.
#
#   entity  `tag.<name>` if your Home Assistant makes tag entities. If it does
#           not, point this at any helper an automation touches on the
#           `tag_scanned` trigger -- an input_button is one line of YAML. Any
#           entity whose state changes will do, which is why a Zigbee button by
#           the kettle works just as well as a sticker.
#
# A phone reports its sensors on the companion app's own schedule -- a minute,
# at best -- so a gate waiting on a step count spends its first minute waiting
# to hear about a walk that is already over. `nudge` is the way out of that: the
# Android companion app answers a notification of `command_update_sensors` by
# reporting everything it has, at once. Tea sends one ten seconds into a break
# and every ten seconds after that, and the steps turn up on the next poll
# rather than on the phone's next minute. Nothing waits on it -- a poke the hub
# will not take is one line on stderr and a break that ends exactly as it would
# have without any of this.
#
#   nudge   the phone's notify service: "notify.mobile_app_<phone>", or the
#           bare "mobile_app_<phone>". Only sent while a break is on screen and
#           only when the steps or the moving are being read -- a phone nobody
#           is listening to is a phone with no reason to be woken. Empty pokes
#           nobody, which is the default.
#
# And the other way round. With `publish` on, tea keeps a sensor on the hub
# saying what it is doing -- "working", "warning", "break", "waiting", "held",
# "off" -- with the next break, the walk so far and today's tally hanging off
# it as attributes; six plain numbers beside it that the hub can graph
# (sensor.tea_worked, _walk, _steps_today, _breaks_today, _postpones_today,
# and binary_sensor.tea_break); and fires a `tea` event at each turn:
# break_start, scan, released, break_end, postpone, and so on, each with
# `what` set to that word. dist/home-assistant/ has a dashboard and the
# automations, ready to paste.
# An automation on `event_type: tea` with `event_data: {what: break_start}` is
# the hall light, the speaker, or the phone. Needs `url` and a token, not the
# tag: reporting works with nfc off. Sent when something changes, not every
# second -- the countdown is `break_ends_at`, and the hub can do that sum.
#
#   publish         "off" (default) or "on".
#   publish_entity  what the hub knows tea as.
[nfc.home_assistant]
url = ""              # e.g. "http://homeassistant.local:8123"
token = ""            # a long-lived access token, from your profile page
entity = ""           # e.g. "tag.living_room"
poll = "2s"           # how often to ask, while a break is up
nudge = ""            # e.g. "notify.mobile_app_pixel" -- tell it to report now
publish = "off"
publish_entity = "sensor.tea"

# The other half of the gate. A tag proves you stood up; it does not prove you
# went anywhere, and a tag within reach of the chair proves nothing at all. With
# this on the page waits for both -- the scan *and* the steps -- and neither one
# alone ends the break. Steps are counted from where you were when the page went
# up, so a phone's daily total is a perfectly good sensor to point at.
#
# Read from the same hub as the tag above, on the same beat, and only while a
# break is on screen. A step count that never arrives -- a phone that syncs late,
# a sensor that is renamed -- ends the break on `grace` like anything else here:
# nothing in this file is allowed to lock a screen.
#
#   mode    "off" (default) or "on". "enabled"/"active" also read as on.
#   count   steps this break wants. 20 is out of the room and back.
#   entity  the sensor holding the count. Any entity whose state is a rising
#           number will do -- a phone, a watch, a Health Connect feed. A total
#           since the phone last rebooted is as good as a daily one: it is
#           only ever measured from, never credited, and a drop moves the mark.
#   sync    "batched" (default) or "live". How promptly the sensor reports.
#           A batched feed -- Health Connect, a watch that uploads when it
#           feels like it -- lags by minutes or hours, so the first rise of a
#           break is the phone catching up on ground covered before the page
#           went up, and it moves the mark instead of counting. The phone's
#           own step counter, sent by the companion app every minute or so,
#           is "live": that same rise is the walk itself and counts. Say
#           "live" only for a sensor that keeps up; said of a lagging one it
#           opens the gate from the chair.
#
# Either way, `nudge` above is what keeps a break from waiting a whole minute
# on a phone that has not got round to mentioning the walk yet.
[nfc.steps]
mode = "off"
count = 20
entity = ""           # e.g. "sensor.pixel_daily_steps"
sync = "batched"

# And a third half, for the hole the other two leave: steps can be earned by a
# phone waved at the desk. Android's activity recognition wants the whole
# body going somewhere, and the companion app reports its word as an entity --
# "walking", "still", "in_vehicle". With this on the page also waits for a
# little time in a moving state, added up over the break. No hardware: switch
# on the "Detected activity" sensor in the companion app and point this at it.
# Read from the hub on the same beat as the steps, and lazily reported by the
# phone -- a minute behind at times -- so `for` is a floor, not a stopwatch,
# and `grace` still ends a break the phone never speaks up for.
#
#   mode    "off" (default) or "on".
#   entity  the sensor, "sensor.<phone>_detected_activity".
#   for     how long in a moving state the break wants, in total. "auto"
#           scales it from the steps above -- a hundred steps asks for thirty
#           seconds, ten steps for five, never less than five nor more than
#           two minutes -- and is thirty seconds when no steps are counted.
#           A duration says it outright.
#   states  which of the sensor's words count as moving.
#
# And a hand is not a walk. Steps that arrive while this sensor has said
# "still" the whole time came from a phone being shaken at the desk; once
# there are twenty of them and half a minute has gone by with no movement
# seen, the page says so -- "Nice try" on the steps badge, with the count
# crossed out -- and the day's tally counts it. The gate is not changed by
# this: it is still waiting for the walk, and the teasing stops the moment the
# phone reports moving.
[nfc.moving]
mode = "off"
entity = ""           # e.g. "sensor.pixel_detected_activity"
for = "auto"
states = ["walking", "on_foot", "running"]

# What to do with the five minutes. Everything above is about making you get up;
# this is the only part about what to do once you have, and a break page that
# says "stand up" and nothing else leaves you standing in the kitchen wondering
# why. Point it at a Home Assistant to-do list -- open the window, unload the
# dishwasher, take the bins out -- and the page shows the first few of them in
# the corner, the open ones first, the finished ones struck through.
#
# Read-only, deliberately: ticking a job off from the laptop you have just been
# sent away from is a claim this has no way of checking, and the list is one tap
# away on the phone in your hand. Tick it off there and the page catches up
# within a few seconds, which is the point of reading it live.
#
# Not part of the gate. A list that cannot be read costs one line on stderr and
# an empty corner; it can never hold your desk, and it can never end a break
# early. What is still to do is always at the top and what has been done is
# underneath it, freshest first: tick a job off and it drops below the open
# ones. Among the open ones, anything with a due date comes first, soonest at
# the top, lit up with the time after its name.
#
# Whatever gets ticked off while a page is up is counted: `tea status` says how
# many today, and `tea dash` has them per day and in total.
#
# Of the lines the corner has, the last two are kept for jobs already done
# whenever there are any: a panel showing nothing but outstanding work is a nag,
# and two struck-through lines are what makes it a record of an afternoon going
# well. When there is little left to do, the finished ones take the slack.
#
# Anything that does not fit is counted on a line of its own -- "3 more", in the
# same grey as the branches -- so what you can see plus what it says it is
# hiding is the whole list, and no number on the page can disagree with the rows
# under it. Under that, once the day has something to report, the day's own
# score: "today · 2 tasks done", below a hairline, because it is a fact about you
# rather than about the list.
#
#   mode    "off" (default) or "on".
#   entity  the list, "todo.<name>". Any to-do entity will do.
#   title   what to call it on the page. Empty means the entity id.
#   show    how many jobs the corner has room for, up to ten. The count of what
#           did not fit does not use one of them.
[nfc.chores]
mode = "off"
entity = ""           # e.g. "todo.household"
title = ""            # e.g. "While you're up"
show = 8

# This file, in a browser. `tea settings` switches this on, writes a token
# into [port] if there is none, and opens the page; saving writes the file
# back and restarts tea, the way `tea reload` does. Reachable from wherever
# [port] listen says. Off by default, because an upgrade must never quietly
# put a file editor on a port.
[settings]
page = "off"
"##;

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
    pub hours: Hours,
    pub long: Long,
    pub sound: crate::sound::Config,
    pub animation: crate::overlay::Anim,
    pub hold: crate::overlay::Hold,
    pub page: crate::overlay::Look,
    pub nfc: crate::nfc::Config,
    pub port: Port,
    pub settings: crate::web::Config,
}

/// `[port]`: the one address tea answers on, and the secret it wants.
///
/// Two things use it -- the tag, when tea does its own listening, and the
/// settings page -- and neither owns it, which is why it is not under either.
/// It used to live under `[nfc]`, and a file that still keeps it there is
/// read the same way; see [`FileConfig::settled`].
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct Port {
    /// `address:port`. Loopback answers only this machine.
    pub listen: String,
    /// Shared secret. Anyone who can reach the port and knows this can end
    /// your break and rewrite your settings, so the port stays shut without it.
    pub token: String,
}

impl Default for Port {
    fn default() -> Self {
        Self { listen: "127.0.0.1:9797".into(), token: String::new() }
    }
}

impl FileConfig {
    /// One address and one token, wherever the file put them.
    ///
    /// `[port]` is where they live now; `[nfc]` is where they lived, and a
    /// file from before still says so. The new place wins when both are
    /// written, the old one is honoured when only it is, and the runtime only
    /// ever reads the copy under `nfc`, so nothing downstream has to know.
    pub fn settled(mut self) -> Self {
        if self.nfc.listen.trim().is_empty() || self.port.listen != Port::default().listen {
            self.nfc.listen = self.port.listen.clone();
        }
        if self.nfc.token.trim().is_empty() || !self.port.token.trim().is_empty() {
            self.nfc.token = self.port.token.clone();
        }
        self
    }
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
    /// Inhibitors that never hold a break: any whose app id or reason
    /// contains one of these, matched without regard to case. For the
    /// things that keep the screen awake for your own reasons -- a
    /// keep-the-screen-on helper while a long job runs -- which are not a
    /// call and should not be treated as one.
    pub ignore: Vec<String>,
}

/// When tea is awake at all.
///
/// Everything else here is about how long you have been working; this is the
/// one setting that cares what time it is. Outside these hours nothing is
/// counted and nothing appears -- an evening film is not a work session with
/// the timer paused, it is not a work session.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Hours {
    pub from: Clock,
    pub to: Clock,
    pub days: Days,
}

/// Every so often, a longer break.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Long {
    /// Every this-many-th break is the long one. Zero for none.
    pub every: u32,
    pub length: Dur,
}

impl Hours {
    /// Whether tea should be doing anything at all at this moment.
    pub fn awake(&self, now: Now) -> bool {
        if !self.days.includes(now.weekday) {
            return false;
        }
        match (self.from.minute(), self.to.minute()) {
            (None, None) => true,
            // One end set and not the other means the other end of the day:
            // `from = "09:00"` on its own is "from nine until midnight", which
            // is what anybody writing only that line means.
            (from, to) => {
                let (from, to) = (from.unwrap_or(0), to.unwrap_or(24 * 60));
                match from < to {
                    true => (from..to).contains(&now.minute),
                    // Wrapped past midnight: 22:00 to 06:00 is one window, not
                    // an empty one, and somebody works those hours.
                    false => now.minute >= from || now.minute < to,
                }
            }
        }
    }

    /// Whether any of this is switched on.
    pub fn set(&self) -> bool {
        !self.days.every_day() || self.from.minute().is_some() || self.to.minute().is_some()
    }

    /// When tea wakes up again, as a phrase that follows whatever said it is
    /// asleep -- "asleep, back at 09:00", "outside hours — back on mon–fri".
    pub fn opens(&self) -> String {
        match (self.from.minute(), self.days.every_day()) {
            (Some(from), true) => format!("back at {}", clock::oclock(from)),
            (Some(from), false) => format!("back at {} on {}", clock::oclock(from), self.days),
            (None, _) => format!("back on {}", self.days),
        }
    }

    /// How it reads in `tea config`.
    pub fn describe(&self) -> String {
        match (self.from.minute(), self.to.minute()) {
            (None, None) => self.days.to_string(),
            (from, to) => format!(
                "{}–{}, {}",
                clock::oclock(from.unwrap_or(0)),
                clock::oclock(to.unwrap_or(24 * 60)),
                self.days
            ),
        }
    }
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
            calls: Calls { warn_after: Dur(d.defer_warn_after), ignore: Vec::new() },
            hours: Hours { from: Clock(None), to: Clock(None), days: Days::all() },
            long: Long { every: d.long_every, length: Dur(d.long_brk) },
            sound: crate::sound::Config::default(),
            animation: crate::overlay::Anim::default(),
            hold: crate::overlay::Hold::default(),
            page: crate::overlay::Look::default(),
            settings: crate::web::Config::default(),
            nfc: crate::nfc::Config::default(),
            port: Port::default(),
        }
        .settled()
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

impl Default for Hours {
    fn default() -> Self {
        FileConfig::default().hours
    }
}

impl Default for Long {
    fn default() -> Self {
        FileConfig::default().long
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
            // One switch, two consequences: the ear opens and the break page
            // stops lifting on the countdown alone. Splitting them would let
            // you configure a break nothing on earth could end.
            require_release: f.nfc.on(),
            release_grace: f.nfc.grace.0,
            long_every: f.long.every,
            long_brk: f.long.length.0,
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
    toml::from_str::<FileConfig>(&text)
        .map(FileConfig::settled)
        .map_err(|e| format!("in {}:\n{e}", path.display()))
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

    // A long break of nothing is not a long break, and one shorter than the
    // ordinary break is a punishment for good behaviour. Either is a typo.
    if c.long_every > 0 && c.long_brk < c.brk {
        notes.push(format!(
            "long.length {} is shorter than break {} — ignoring it",
            human(c.long_brk),
            human(c.brk)
        ));
        c.long_every = 0;
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
                // Checked, for the reason `parse` is bounded just below: a
                // number nobody could have meant has to come back as a bad
                // setting, not wrap into a work interval of a few seconds.
                u64::try_from(n)
                    .ok()
                    .and_then(|n| n.checked_mul(60))
                    .map(|secs| Dur(Duration::from_secs(secs)))
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
    } else if s.is_multiple_of(3600) {
        format!("{}h", s / 3600)
    } else if s.is_multiple_of(60) {
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

    fn at(hour: u32, weekday: u32) -> Now {
        Now { minute: hour * 60, weekday }
    }

    fn hours(from: &str, to: &str, days: &str) -> Hours {
        Hours {
            from: Clock::parse(from).unwrap(),
            to: Clock::parse(to).unwrap(),
            days: Days::parse(days).unwrap(),
        }
    }

    #[test]
    fn working_hours_are_off_until_they_are_set() {
        let always = Hours::default();
        assert!(!always.set());
        for hour in [0, 3, 9, 17, 23] {
            for day in 0..7 {
                assert!(always.awake(at(hour, day)), "{hour}:00 on day {day}");
            }
        }
    }

    #[test]
    fn a_working_day_has_two_ends_and_a_weekend() {
        let nine_to_six = hours("09:00", "18:00", "mon-fri");
        assert!(nine_to_six.set());
        assert!(nine_to_six.awake(at(9, 0)), "nine is inside");
        assert!(nine_to_six.awake(at(17, 4)));
        assert!(!nine_to_six.awake(at(8, 0)), "before it starts");
        assert!(!nine_to_six.awake(at(18, 0)), "six is the end, not the last hour");
        assert!(!nine_to_six.awake(at(20, 2)), "an evening film is not a work session");
        assert!(!nine_to_six.awake(at(11, 5)), "Saturday");
        assert!(!nine_to_six.awake(at(11, 6)), "Sunday");
    }

    #[test]
    fn a_window_that_runs_backwards_wraps_midnight() {
        // Somebody works these hours, and reading it as an empty window would
        // switch tea off for them entirely.
        let night = hours("22:00", "06:00", "all");
        assert!(night.awake(at(23, 0)));
        assert!(night.awake(at(2, 0)));
        assert!(!night.awake(at(12, 0)));
        assert!(!night.awake(at(6, 0)), "six is the end");
    }

    #[test]
    fn one_end_of_the_day_means_the_other_end_is_the_day() {
        // `from = "09:00"` on its own is what somebody writes for "not before
        // nine", and reading the missing end as midnight-to-midnight would
        // make the line do nothing at all.
        let after_nine = hours("09:00", "off", "all");
        assert!(!after_nine.awake(at(8, 0)));
        assert!(after_nine.awake(at(9, 0)) && after_nine.awake(at(23, 0)));

        let before_six = hours("off", "18:00", "all");
        assert!(before_six.awake(at(0, 0)) && before_six.awake(at(17, 0)));
        assert!(!before_six.awake(at(18, 0)));

        // Days on their own, with no hours at all, still count as set.
        let weekdays = hours("off", "off", "mon-fri");
        assert!(weekdays.set());
        assert!(weekdays.awake(at(3, 0)) && !weekdays.awake(at(11, 6)));
    }

    #[test]
    fn a_long_break_shorter_than_the_short_one_is_a_typo() {
        let mut cfg = tea_core::Config {
            brk: Duration::from_secs(300),
            long_every: 4,
            long_brk: Duration::from_secs(60),
            ..tea_core::Config::default()
        };
        let notes = reconcile(&mut cfg).unwrap();
        assert_eq!(cfg.long_every, 0, "a shorter long break is switched off, not honoured");
        assert!(notes.iter().any(|n| n.contains("long.length")), "and said out loud: {notes:?}");
    }

    #[test]
    fn shipped_default_file_parses_to_the_code_defaults() {
        let parsed: FileConfig = toml::from_str(DEFAULT_FILE).expect("default file must parse");
        let parsed = parsed.settled();
        let mut got: tea_core::Config = parsed.into();
        assert_eq!(got, tea_core::Config::default());
        assert!(reconcile(&mut got).unwrap().is_empty(), "defaults must need no clamping");
    }

    /// Every word of it, not just the parts that reach `tea_core::Config`.
    ///
    /// The settings page reads this file to say what a setting *would* be
    /// where yours has never said a word about it -- that is the whole of how
    /// it can offer a setting your file has not got. A block in here that has
    /// drifted from the code is the page stating a default that is not one.
    /// Compared through `Debug`, because these structs are read from a file
    /// and never compared to each other anywhere else.
    #[test]
    fn the_shipped_file_states_the_code_defaults_in_full() {
        let shipped: FileConfig = toml::from_str(DEFAULT_FILE).expect("default file must parse");
        // Settled on both sides: `[port] listen` has an older home under
        // `[nfc]`, and which of the two a file happens to fill in is not a
        // difference of default. See [`FileConfig::settled`].
        assert_eq!(
            format!("{:#?}", shipped.settled()),
            format!("{:#?}", FileConfig::default().settled()),
            "the shipped config file and the code no longer agree on the defaults"
        );
    }

    #[test]
    fn partial_file_keeps_defaults_for_everything_else() {
        let parsed: FileConfig = toml::from_str("work = \"50m\"\n").unwrap();
        let got: tea_core::Config = parsed.into();
        assert_eq!(got.work, Duration::from_secs(50 * 60));
        assert_eq!(got.brk, tea_core::Config::default().brk);
    }

    #[test]
    fn the_screen_is_only_held_when_the_file_asks_for_it() {
        use crate::overlay::Grip;

        // The shipped file, and an empty one, both leave you able to walk away.
        let shipped: FileConfig = toml::from_str(DEFAULT_FILE).unwrap();
        assert_eq!(shipped.hold.mode, Grip::Soft);
        assert_eq!(toml::from_str::<FileConfig>("").unwrap().hold.mode, Grip::Soft);

        let asked: FileConfig =
            toml::from_str("[hold]\nmode = \"insist\"\nrecheck = \"250ms\"\n").unwrap();
        assert_eq!(asked.hold.mode, Grip::Insist);
        assert_eq!(asked.hold.recheck.0, Duration::from_millis(250));

        // A mode nobody implements must say so rather than quietly meaning soft.
        let err = toml::from_str::<FileConfig>("[hold]\nmode = \"maximum\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("maximum"), "{err}");
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

    #[test]
    fn the_port_is_read_from_wherever_the_file_put_it() {
        // A file from before there was a [port]: listen and token under [nfc].
        let old: FileConfig = toml::from_str("[nfc]\nlisten = \"0.0.0.0:9797\"\ntoken = \"abc\"\n").unwrap();
        let old = old.settled();
        assert_eq!(old.nfc.listen, "0.0.0.0:9797");
        assert_eq!(old.nfc.token, "abc");

        // Today's shape.
        let new: FileConfig = toml::from_str("[port]\nlisten = \"0.0.0.0:9898\"\ntoken = \"xyz\"\n").unwrap();
        let new = new.settled();
        assert_eq!(new.nfc.listen, "0.0.0.0:9898");
        assert_eq!(new.nfc.token, "xyz");

        // Both, half-migrated: the new place wins where it says something.
        let both: FileConfig =
            toml::from_str("[nfc]\nlisten = \"0.0.0.0:9797\"\ntoken = \"abc\"\n[port]\ntoken = \"xyz\"\n").unwrap();
        let both = both.settled();
        assert_eq!(both.nfc.listen, "0.0.0.0:9797", "the old listen, as [port] left it alone");
        assert_eq!(both.nfc.token, "xyz", "the new token");

        // Neither: loopback, no token, same as the starter file.
        let none: FileConfig = toml::from_str("").unwrap();
        let none = none.settled();
        assert_eq!(none.nfc.listen, "127.0.0.1:9797");
        assert!(none.nfc.token.is_empty());
        assert_eq!(FileConfig::default().nfc.listen, "127.0.0.1:9797");
    }
}

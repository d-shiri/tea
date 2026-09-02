//! Telling Home Assistant what tea is doing.
//!
//! The tag and the steps are the hub talking to tea. This is tea talking back:
//! one sensor whose state says what tea is up to -- `working`, `break`,
//! `waiting`, `off` -- with the next break and the walk so far hanging off it
//! as attributes; a handful of plain numbers beside it, because a number with
//! a unit is the only thing the hub knows how to graph; and one event fired
//! at each turn of the break so an automation can light the hall the moment
//! the page goes up and ring the speaker the moment the walk is in.
//!
//! Sent when something changes and not otherwise. The hub is told about a new
//! state, a scan, a step count that moved, and once a minute that the clock is
//! still running; it is not told the time left on the countdown every second,
//! because it can work that out from `break_ends_at` for itself, and a hub
//! written to three hundred times per break is a hub whose owner turns this
//! off. Nothing here can hold the desk: a hub that cannot be reached costs one
//! line on stderr and a retry later, and the break carries on exactly as it
//! would have.

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::nfc::{HomeAssistant, split_url};

/// The event type every turn is fired as. One type with a `what` inside it
/// rather than one type per turn, so a single trigger catches them all and
/// the automation gets to choose.
pub const EVENT: &str = "tea";

/// How long one report is given before it is written off.
const SEND_TIMEOUT: u32 = 10;
/// A state reply is a copy of the state plus some headers; this is plenty.
const REPLY_CAP: usize = 16 * 1024;
/// After a failure, this long before the hub is bothered again. Long enough
/// that a hub that is down for the afternoon costs a connection attempt every
/// half minute and not every second; short enough that one back up is told
/// about the next break before the page has lifted.
const RETRY_AFTER: Duration = Duration::from_secs(30);
/// Events kept while the hub is unreachable. Past this the oldest go: a
/// morning's breaks arriving all at once on a hub that has just come back is
/// not information, it is a light show.
const QUEUE_CAP: usize = 20;
/// Everything is said again this often, changed or not. Entities set through
/// the API are the hub's until it restarts, and then they are nobody's: a
/// hub that came back at two o'clock would otherwise show no tea at all until
/// something happened to change.
const REFRESH: Duration = Duration::from_secs(5 * 60);

/// What tea is doing, in one word the hub can switch on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum State {
    /// The clock is running towards a break.
    #[default]
    Working,
    /// A break is about to start, and the warning is up.
    Warning,
    /// A break is owed and being held back by a call or a film.
    Held,
    /// The page is up and the countdown is running.
    Break,
    /// The countdown has run out and the page is waiting on the tag or the walk.
    Waiting,
    /// Asleep: `tea off`, or outside working hours.
    Off,
}

impl State {
    pub fn word(self) -> &'static str {
        match self {
            State::Working => "working",
            State::Warning => "warning",
            State::Held => "held",
            State::Break => "break",
            State::Waiting => "waiting",
            State::Off => "off",
        }
    }

    /// Whether the page is on screen.
    fn breaking(self) -> bool {
        matches!(self, State::Break | State::Waiting)
    }
}

/// Everything the hub is told about the state, as one value that can be
/// compared with the last one: it goes when it differs, and not otherwise.
///
/// Times are moments rather than countdowns for the same reason. "The break
/// ends at 14:07" is true for the whole break and needs saying once; "four
/// minutes left" is true for a second.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    pub state: State,
    /// When the next break falls due, unix seconds. Absent during a break.
    pub next_break_at: Option<u64>,
    /// When the page lifts on its own, unix seconds. Absent between breaks and
    /// once the page is waiting on the tag, when nobody knows.
    pub break_ends_at: Option<u64>,
    /// How long the break on screen runs for, in seconds. Zero between breaks.
    pub break_len: u64,
    /// Whether the break on screen is one of the long ones.
    pub long: bool,
    /// Work banked towards the next break, in whole minutes. Whole minutes on
    /// purpose: this is what makes a report differ once a minute while nothing
    /// else changes, which is the heartbeat that says tea is alive.
    pub worked_min: u64,
    pub tag_scanned: bool,
    pub steps_walked: u32,
    /// Steps this break asks for. Zero when the walk is not part of it.
    pub steps_needed: u32,
    /// Seconds the phone has said you were moving, and how many are wanted.
    /// Zero `moving_needed` when that is not part of it.
    pub moving_secs: u32,
    pub moving_needed: u32,
    pub postpones_left: u32,
    pub breaks_today: u32,
    pub steps_today: u32,
    pub credited_today: u32,
    pub postponed_today: u32,
    /// Breaks today in which the steps came from a hand.
    pub cheats_today: u32,
    /// Why tea is asleep, when it is, in the words it printed.
    pub why_off: Option<String>,
}

impl Report {
    /// The body of the main sensor, as Home Assistant wants it.
    pub fn body(&self) -> Value {
        let mut attrs = json!({
            "friendly_name": "tea",
            "icon": "mdi:tea",
            "break_len": self.break_len,
            "long": self.long,
            "worked_min": self.worked_min,
            "tag_scanned": self.tag_scanned,
            "steps_walked": self.steps_walked,
            "steps_needed": self.steps_needed,
            "moving_secs": self.moving_secs,
            "moving_needed": self.moving_needed,
            "postpones_left": self.postpones_left,
            "breaks_today": self.breaks_today,
            "steps_today": self.steps_today,
            "credited_today": self.credited_today,
            "postponed_today": self.postponed_today,
            "cheats_today": self.cheats_today,
        });
        // Timestamps as ISO 8601 rather than seconds: that is what the hub's
        // own `as_datetime` and every timestamp card expect, and a number
        // that is secretly a date is the kind of thing that costs an evening.
        attrs["next_break_at"] = self.next_break_at.map_or(Value::Null, |t| iso(t).into());
        attrs["break_ends_at"] = self.break_ends_at.map_or(Value::Null, |t| iso(t).into());
        attrs["why_off"] = self.why_off.clone().map_or(Value::Null, Value::String);
        json!({ "state": self.state.word(), "attributes": attrs })
    }

    /// Every entity this report sets, and what to set it to. `entity` is the
    /// main sensor as configured; `name` is its object id, which the numbers
    /// beside it are named after -- `sensor.tea` gets `sensor.tea_steps_today`.
    ///
    /// The numbers exist because the hub cannot graph an attribute. Each has a
    /// unit and a state class, which is what the recorder needs to keep
    /// long-term statistics for it: `total_increasing` for the counts that
    /// start again at midnight, `measurement` for the ones that go up and
    /// down.
    pub fn bodies(&self, entity: &str, name: &str) -> Vec<(String, Value)> {
        let number = |n: u64, unit: &str, class: &str, called: &str, icon: &str| {
            // The state is a string, whatever it holds: the hub's API takes
            // nothing else, and answers a bare number with 400.
            json!({
                "state": n.to_string(),
                "attributes": {
                    "friendly_name": called,
                    "icon": icon,
                    "unit_of_measurement": unit,
                    "state_class": class,
                },
            })
        };
        let mut walk = number(
            u64::from(self.steps_walked),
            "steps",
            "measurement",
            "tea walk",
            "mdi:shoe-print",
        );
        walk["attributes"]["steps_needed"] = self.steps_needed.into();
        vec![
            (entity.to_string(), self.body()),
            (
                format!("binary_sensor.{name}_break"),
                json!({
                    "state": if self.state.breaking() { "on" } else { "off" },
                    "attributes": { "friendly_name": "tea break", "icon": "mdi:tea" },
                }),
            ),
            (
                format!("sensor.{name}_worked"),
                number(self.worked_min, "min", "measurement", "tea worked", "mdi:progress-clock"),
            ),
            (format!("sensor.{name}_walk"), walk),
            (
                format!("sensor.{name}_steps_today"),
                number(
                    u64::from(self.steps_today),
                    "steps",
                    "total_increasing",
                    "tea steps today",
                    "mdi:walk",
                ),
            ),
            (
                format!("sensor.{name}_breaks_today"),
                number(
                    u64::from(self.breaks_today),
                    "breaks",
                    "total_increasing",
                    "tea breaks today",
                    "mdi:coffee",
                ),
            ),
            (
                format!("sensor.{name}_postpones_today"),
                number(
                    u64::from(self.postponed_today),
                    "postpones",
                    "total_increasing",
                    "tea postpones today",
                    "mdi:timer-sand",
                ),
            ),
            (
                format!("sensor.{name}_cheats_today"),
                number(
                    u64::from(self.cheats_today),
                    "tries",
                    "total_increasing",
                    "tea nice tries",
                    "mdi:emoticon-devil-outline",
                ),
            ),
        ]
    }
}

/// Unix seconds as `2026-09-02T14:07:00+00:00`.
///
/// Civil-from-days, the way every date library does it underneath; written
/// out here rather than pulling one in for a single attribute.
pub fn iso(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    let secs = unix % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}+00:00",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    )
}

/// One thing to send: where, and what.
#[derive(Debug, Clone)]
struct Job {
    path: String,
    body: Value,
    /// The entity this sets, when it sets one -- so a state that fails to
    /// send can be put back rather than lost, and one that lands can be
    /// remembered as what the hub now shows.
    entity: Option<String>,
}

/// The connection to the hub, and the queue of things to tell it.
///
/// One at a time, on the main loop, like the poll: two sockets to the same
/// hub in the same second is one more than a sensor that changes at walking
/// pace deserves.
pub struct Publisher {
    inner: Rc<Inner>,
}

struct Inner {
    host: String,
    port: u16,
    tls: bool,
    base: String,
    events_path: String,
    token: String,
    /// The main sensor, as configured.
    entity: String,
    /// Its object id, which the numbers beside it are named after.
    name: String,
    /// What the hub last accepted, per entity. A body equal to the one here
    /// is not sent again.
    sent: RefCell<HashMap<String, Value>>,
    /// States waiting their turn, one per entity. Replaced, not queued: only
    /// the newest value of a sensor is worth saying.
    pending: RefCell<Vec<Job>>,
    /// Events wait their turn in order, and every one is sent -- a scan that
    /// was followed by a break ending is two things, not one.
    events: RefCell<VecDeque<Job>>,
    busy: Cell<bool>,
    /// Said once, when the hub stops answering, and once more when it starts.
    down: Cell<bool>,
    retry_at: Cell<Option<Instant>>,
    /// When everything was last said regardless -- see `REFRESH`.
    refreshed: Cell<Instant>,
}

impl Publisher {
    pub fn new(cfg: &HomeAssistant) -> Result<Self, String> {
        let token = cfg.secret().map_err(|e| {
            format!("{e} (a long-lived access token, from the bottom of your profile page)")
        })?;
        if let Some(why) = cfg.publish_misconfigured() {
            return Err(why);
        }
        let (host, port, tls, base) = split_url(&cfg.url)?;
        let entity = cfg.publish_entity.trim().to_string();
        let name = entity.split_once('.').map_or("tea", |(_, name)| name).to_string();
        Ok(Self {
            inner: Rc::new(Inner {
                host,
                port,
                tls,
                events_path: format!("{base}/api/events/{EVENT}"),
                base,
                token,
                entity,
                name,
                sent: RefCell::new(HashMap::new()),
                pending: RefCell::new(Vec::new()),
                events: RefCell::new(VecDeque::new()),
                busy: Cell::new(false),
                down: Cell::new(false),
                retry_at: Cell::new(None),
                refreshed: Cell::new(Instant::now()),
            }),
        })
    }

    pub fn entity(&self) -> &str {
        &self.inner.entity
    }

    /// The numbers published beside the main sensor, for saying so.
    pub fn siblings(&self) -> usize {
        Report::default().bodies(&self.inner.entity, &self.inner.name).len() - 1
    }

    /// This is what tea looks like now. Each entity is sent if it differs
    /// from what the hub was last told and dropped on the floor otherwise,
    /// which is what lets the engine call this every tick without thinking
    /// about it.
    pub fn report(&self, report: Report) {
        let inner = &self.inner;
        if inner.refreshed.get().elapsed() >= REFRESH {
            inner.sent.borrow_mut().clear();
            inner.refreshed.set(Instant::now());
        }
        {
            let sent = inner.sent.borrow();
            let mut pending = inner.pending.borrow_mut();
            for (entity, body) in report.bodies(&inner.entity, &inner.name) {
                let waiting = pending.iter().position(|j| j.entity.as_deref() == Some(&entity));
                if sent.get(&entity) == Some(&body) {
                    // Back to what the hub already shows: anything queued for
                    // this entity is now a lie about the past.
                    if let Some(i) = waiting {
                        pending.remove(i);
                    }
                    continue;
                }
                match waiting {
                    Some(i) => pending[i].body = body,
                    None => pending.push(Job {
                        path: format!("{}/api/states/{entity}", inner.base),
                        body,
                        entity: Some(entity),
                    }),
                }
            }
        }
        Inner::kick(inner);
    }

    /// Something happened. `what` is the word an automation switches on;
    /// `data` is whatever goes with it.
    pub fn event(&self, what: &str, mut data: Value) {
        data["what"] = Value::String(what.to_string());
        let mut events = self.inner.events.borrow_mut();
        if events.len() >= QUEUE_CAP {
            events.pop_front();
        }
        events.push_back(Job { path: self.inner.events_path.clone(), body: data, entity: None });
        drop(events);
        Inner::kick(&self.inner);
    }
}

impl Inner {
    /// Send the next thing, if nothing is being sent and the hub is not being
    /// left alone after a failure. Calls itself again when the send lands, so
    /// one kick empties the queue.
    fn kick(this: &Rc<Self>) {
        if this.busy.get() {
            return;
        }
        if let Some(at) = this.retry_at.get()
            && Instant::now() < at
        {
            return;
        }
        // Events first: they say what happened, the states say how things
        // stand now, and an automation reading both wants them in that order.
        let job = match this.events.borrow_mut().pop_front() {
            Some(job) => job,
            None => {
                let mut pending = this.pending.borrow_mut();
                if pending.is_empty() {
                    return;
                }
                pending.remove(0)
            }
        };
        this.busy.set(true);
        let this = Rc::clone(this);
        glib::MainContext::default().spawn_local(async move {
            let outcome = send(&this, &job.path, &job.body).await;
            this.busy.set(false);
            match outcome {
                Ok(()) => {
                    this.retry_at.set(None);
                    if let Some(entity) = job.entity {
                        this.sent.borrow_mut().insert(entity, job.body);
                    }
                    if this.down.replace(false) {
                        println!("[ha]    reporting again — the hub is back");
                    }
                    Inner::kick(&this);
                }
                Err(why) => {
                    if !this.down.replace(true) {
                        eprintln!("tea: cannot report to Home Assistant — {why}");
                    }
                    this.retry_at.set(Some(Instant::now() + RETRY_AFTER));
                    // A failed state goes back to the front of the line,
                    // unless a newer one for the same entity has arrived in
                    // the meantime, in which case the newer one is the truth.
                    if job.entity.is_some() {
                        let mut pending = this.pending.borrow_mut();
                        if !pending.iter().any(|j| j.entity == job.entity) {
                            pending.insert(0, job);
                        }
                    } else {
                        let mut events = this.events.borrow_mut();
                        if events.len() < QUEUE_CAP {
                            events.push_front(job);
                        }
                    }
                }
            }
        });
    }
}

/// Fire one event, now, and say how it went -- for `tea --probe`, so a token
/// that cannot write is caught at the desk rather than noticed as a hall light
/// that never came on.
pub fn probe(cfg: &HomeAssistant) -> Result<String, String> {
    let publisher = Publisher::new(cfg)?;
    let body = json!({ "what": "probe" });
    glib::MainContext::default()
        .block_on(send(&publisher.inner, &publisher.inner.events_path, &body))
        .map(|()| format!("fired a `{EVENT}` event with what = \"probe\""))
}

/// One POST, on the main loop.
async fn send(this: &Inner, path: &str, body: &Value) -> Result<(), String> {
    let client = gio::SocketClient::new();
    client.set_tls(this.tls);
    client.set_timeout(SEND_TIMEOUT);

    let conn = client
        .connect_to_host_future(&format!("{}:{}", this.host, this.port), this.port)
        .await
        .map_err(|e| format!("cannot reach {}:{} — {e}", this.host, this.port))?;

    let payload = body.to_string();
    // HTTP/1.0, like the poll: the reply cannot then be chunked, and the
    // only thing read from it is the status line.
    let request = format!(
        "POST {} HTTP/1.0\r\nHost: {}\r\nAuthorization: Bearer {}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{}",
        path,
        this.host,
        this.token,
        payload.len(),
        payload
    );
    conn.output_stream()
        .write_all_future(request.into_bytes(), glib::Priority::DEFAULT)
        .await
        .map_err(|(_, e)| format!("cannot send: {e}"))?;

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

    accepted(&raw)
}

/// Whether the hub took it, and if not, why in words somebody can act on.
fn accepted(raw: &[u8]) -> Result<(), String> {
    let text = String::from_utf8_lossy(raw);
    let head = text
        .split("\r\n\r\n")
        .next()
        .ok_or_else(|| "Home Assistant answered with something that is not HTTP".to_string())?;
    let status = head.lines().next().unwrap_or("").split_whitespace().nth(1).unwrap_or("");
    match status {
        "200" | "201" => Ok(()),
        "401" | "403" => Err("Home Assistant refused the token (nfc.home_assistant.token)".into()),
        "400" => Err("Home Assistant rejected the entity id (nfc.home_assistant.publish_entity)".into()),
        "404" => Err("Home Assistant has no API at that url (nfc.home_assistant.url)".into()),
        "" => Err("Home Assistant answered with something that is not HTTP".into()),
        other => Err(format!("Home Assistant answered {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_moment_is_written_the_way_the_hub_reads_it() {
        assert_eq!(iso(0), "1970-01-01T00:00:00+00:00");
        // 2026-09-03 14:07:05 UTC
        assert_eq!(iso(1_788_444_425), "2026-09-03T14:07:05+00:00");
        // The last second of a leap-year February.
        assert_eq!(iso(1_709_251_199), "2024-02-29T23:59:59+00:00");
    }

    #[test]
    fn the_body_names_the_state_and_the_next_break() {
        let report = Report {
            state: State::Working,
            next_break_at: Some(1_788_444_425),
            worked_min: 12,
            postpones_left: 2,
            ..Report::default()
        };
        let body = report.body();
        assert_eq!(body["state"], "working");
        assert_eq!(body["attributes"]["next_break_at"], "2026-09-03T14:07:05+00:00");
        assert_eq!(body["attributes"]["break_ends_at"], Value::Null);
        assert_eq!(body["attributes"]["worked_min"], 12);
        assert_eq!(body["attributes"]["friendly_name"], "tea");
    }

    #[test]
    fn a_break_says_when_it_ends_and_how_the_walk_is_going() {
        let report = Report {
            state: State::Break,
            break_ends_at: Some(1_788_444_425),
            break_len: 300,
            steps_walked: 7,
            steps_needed: 20,
            tag_scanned: true,
            ..Report::default()
        };
        let body = report.body();
        assert_eq!(body["state"], "break");
        assert_eq!(body["attributes"]["break_ends_at"], "2026-09-03T14:07:05+00:00");
        assert_eq!(body["attributes"]["steps_walked"], 7);
        assert_eq!(body["attributes"]["steps_needed"], 20);
        assert_eq!(body["attributes"]["tag_scanned"], true);
    }

    #[test]
    fn asleep_says_why() {
        let report =
            Report { state: State::Off, why_off: Some("off for another 1h".into()), ..Report::default() };
        let body = report.body();
        assert_eq!(body["state"], "off");
        assert_eq!(body["attributes"]["why_off"], "off for another 1h");
    }

    #[test]
    fn the_numbers_are_named_after_the_sensor_and_carry_units() {
        let report = Report {
            state: State::Waiting,
            worked_min: 3,
            steps_walked: 41,
            steps_needed: 100,
            steps_today: 812,
            breaks_today: 4,
            postponed_today: 1,
            cheats_today: 2,
            ..Report::default()
        };
        let bodies = report.bodies("sensor.desk", "desk");
        let of = |entity: &str| {
            bodies.iter().find(|(e, _)| e == entity).map(|(_, b)| b.clone()).unwrap()
        };
        assert_eq!(bodies[0].0, "sensor.desk");
        assert_eq!(of("binary_sensor.desk_break")["state"], "on");
        assert_eq!(of("sensor.desk_worked")["state"], "3");
        assert_eq!(of("sensor.desk_worked")["attributes"]["unit_of_measurement"], "min");
        assert_eq!(of("sensor.desk_walk")["state"], "41");
        assert_eq!(of("sensor.desk_walk")["attributes"]["steps_needed"], 100);
        assert_eq!(of("sensor.desk_steps_today")["state"], "812");
        assert_eq!(
            of("sensor.desk_steps_today")["attributes"]["state_class"],
            "total_increasing"
        );
        assert_eq!(of("sensor.desk_breaks_today")["state"], "4");
        assert_eq!(of("sensor.desk_postpones_today")["state"], "1");
        assert_eq!(of("sensor.desk_cheats_today")["state"], "2");
        // Between breaks the binary sensor is off, whatever else is going on.
        let working = Report { state: State::Warning, ..Report::default() };
        let bodies = working.bodies("sensor.tea", "tea");
        assert_eq!(bodies[1].0, "binary_sensor.tea_break");
        assert_eq!(bodies[1].1["state"], "off");
    }

    #[test]
    fn the_reply_is_read_for_what_went_wrong() {
        assert_eq!(accepted(b"HTTP/1.0 200 OK\r\n\r\n{}"), Ok(()));
        assert_eq!(accepted(b"HTTP/1.0 201 Created\r\n\r\n{}"), Ok(()));
        assert!(accepted(b"HTTP/1.0 401 Unauthorized\r\n\r\n").unwrap_err().contains("token"));
        assert!(accepted(b"HTTP/1.0 400 Bad Request\r\n\r\n").unwrap_err().contains("entity id"));
        assert!(accepted(b"").is_err());
        assert!(accepted(b"garbage").is_err());
    }
}

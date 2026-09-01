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

use crate::config::{self, human};
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use serde::{Deserialize, de};
use std::cell::{Cell, RefCell};
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

/// Requests are one small packet. Anything past this is not a tag.
const REQUEST_CAP: usize = 8 * 1024;
/// A connection that has not finished asking by now never will.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

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
    /// `address:port`. Loopback answers only this machine, which is useful for
    /// trying it out and useless for an actual tag — a phone in another room
    /// needs an address it can reach.
    pub listen: String,
    /// Shared secret. Anyone who can reach the port and knows this can end your
    /// break, so it is not optional when the ear is open.
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
            listen: "127.0.0.1:9797".into(),
            token: String::new(),
            grace: Grace(Duration::from_secs(10 * 60)),
            prompt: "Scan the tag to get your desk back".into(),
            url: String::new(),
            home_assistant: HomeAssistant::default(),
            steps: Steps::default(),
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
}

impl Default for Steps {
    fn default() -> Self {
        // Twenty steps is a walk out of the room and back to the doorway. Small
        // enough that nobody games it by shuffling, large enough that it cannot
        // be done from the chair.
        Self { mode: Mode::Off, count: 20, entity: String::new() }
    }
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
                u64::try_from(n)
                    .ok()
                    .map(|n| Grace(Duration::from_secs(n * 60)))
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
}

impl Default for HomeAssistant {
    fn default() -> Self {
        Self {
            url: String::new(),
            token: String::new(),
            token_file: PathBuf::new(),
            entity: String::new(),
            poll: crate::config::Dur(Duration::from_secs(2)),
        }
    }
}

/// What the token is called, in a file and in the environment.
pub const TOKEN_VAR: &str = "TEA_HA_TOKEN";

impl HomeAssistant {
    pub fn on(&self) -> bool {
        !self.url.trim().is_empty() && !self.entity.trim().is_empty()
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
pub fn listen(cfg: &Config, link: Rc<Link>) -> Result<Ear, String> {
    if cfg.token.trim().is_empty() {
        return Err("nfc.token is empty — anyone who can reach the port could end your break \
                    (tea set-nfc on writes one)"
            .into());
    }

    let addr: SocketAddr = cfg.listen.parse().map_err(|_| {
        format!("nfc.listen: {:?} is not an address:port, like \"0.0.0.0:9797\"", cfg.listen)
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
    service.connect_incoming(move |_, conn, _| {
        greet(conn.clone(), Rc::clone(&token), Rc::clone(&link));
        // Handled: nothing else is listening for these.
        true
    });
    service.start();

    Ok(Ear { _service: service, addr })
}

/// Read one request, answer it, hang up.
fn greet(conn: gio::SocketConnection, token: Rc<String>, link: Rc<Link>) {
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
            let reply = answer(&raw, &token, &link, &who);
            // Best effort: a phone that has already walked out of range is not
            // an error worth reporting, and the scan itself is already counted.
            out.write_all_async(reply.into_bytes(), glib::Priority::DEFAULT, gio::Cancellable::NONE, move |_| {
                let _ = conn.close(gio::Cancellable::NONE);
            });
        }),
    );
}

/// Accumulate until the headers end, the cap is hit, or the client stops
/// talking. Boxed rather than generic so it can call itself.
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
                let ended = acc.windows(4).any(|w| w == b"\r\n\r\n");
                if ended || acc.len() >= REQUEST_CAP {
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

/// Work out what was asked and produce the whole response.
fn answer(raw: &[u8], token: &str, link: &Link, who: &str) -> String {
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
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            b'%' if i + 2 < raw.len() => {
                match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    Ok(byte) => {
                        out.push(byte as char);
                        i += 3;
                    }
                    Err(_) => {
                        out.push('%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(' ');
                i += 1;
            }
            b => {
                out.push(b as char);
                i += 1;
            }
        }
    }
    out
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
}

impl Walker {
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
                match self.settled {
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
    fn marked(&self) -> bool {
        self.settled
    }

    fn walked(&self) -> u32 {
        // Sensors report floats, people walk in whole steps, and rounding up
        // would hand out a step nobody took.
        self.walked as u32
    }

    /// Between breaks there is nothing to count and nothing worth remembering.
    fn forget(&mut self) {
        *self = Self::default();
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
            walker: RefCell::new(Walker::default()),
            complained: Cell::new(false),
            misses: Cell::new(0),
            lost: Cell::new(false),
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

    let ask = Rc::new(Ask {
        host,
        port,
        tls,
        path: format!("{base}/api/states/{entity}"),
        token,
        entity,
        legs,
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
            return glib::ControlFlow::Continue;
        }
        poll(Rc::clone(&ask));
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
        ask.busy.set(false);
        settle(&ask, tag, steps);
    });
}

async fn fetch(ask: &Ask, path: &str, entity: &str) -> Result<String, String> {
    let client = gio::SocketClient::new();
    client.set_tls(ask.tls);
    client.set_timeout(ASK_TIMEOUT);

    let conn = client
        .connect_to_host_future(&format!("{}:{}", ask.host, ask.port), ask.port)
        .await
        .map_err(|e| format!("cannot reach {}:{} — {e}", ask.host, ask.port))?;

    // HTTP/1.0 on purpose: it cannot be answered with a chunked body, which
    // saves unpicking one for the sake of a forty-byte string.
    let request = format!(
        "GET {} HTTP/1.0\r\nHost: {}\r\nAuthorization: Bearer {}\r\n\
         Accept: application/json\r\nConnection: close\r\n\r\n",
        path, ask.host, ask.token
    );
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

    read_state(&raw, entity)
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

/// What one answer means for the break on screen.
fn settle(ask: &Ask, answer: Result<String, String>, steps: Option<Result<String, String>>) {
    settle_tag(ask, answer);
    if let (Some(legs), Some(answer)) = (&ask.legs, steps) {
        settle_steps(ask, legs, answer);
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
fn split_url(url: &str) -> Result<(String, u16, bool, String), String> {
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
        .map_err(|_| format!("nfc.listen: {:?} is not an address:port", cfg.listen))?;
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
        answer(format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes(), "s3cret", link, "test")
    }

    #[test]
    fn the_right_token_on_a_waiting_break_unlocks_it() {
        let link = link_with(Desk { breaking: true, waiting: true, ..Desk::default() });
        assert!(get(&link, "/unlock?token=s3cret").starts_with("HTTP/1.1 200"));
        assert!(link.take_scan(), "the tick has a scan waiting for it");
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
        assert!(answer(raw.as_bytes(), "s3cret", &link, "test").starts_with("HTTP/1.1 200"));
    }

    #[test]
    fn a_browser_gets_a_page_and_a_hub_gets_a_line() {
        let link = link_with(Desk { breaking: true, waiting: true, ..Desk::default() });
        let phone = "GET /unlock?token=s3cret HTTP/1.1\r\nAccept: text/html,*/*\r\n\r\n";
        assert!(answer(phone.as_bytes(), "s3cret", &link, "test").contains("<!doctype html>"));
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
            let reply = answer(raw.as_bytes(), "s3cret", &link, "test");
            assert!(reply.starts_with("HTTP/1.1 4"), "{raw:?} → {reply}");
        }
        assert!(!link.take_scan());
    }

    #[test]
    fn a_token_with_awkward_characters_survives_the_query_string() {
        let link = link_with(Desk { breaking: true, waiting: true, ..Desk::default() });
        let raw = "GET /unlock?token=a%20b%2Bc HTTP/1.1\r\n\r\n";
        assert!(answer(raw.as_bytes(), "a b+c", &link, "test").starts_with("HTTP/1.1 200"));
    }

    #[test]
    fn the_tag_url_follows_whatever_the_front_door_is() {
        let mut cfg = Config { token: "t0ken".into(), ..Config::default() };
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
            legs: None,
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
        let steps = |value: &str| settle(&ask, Ok("9:00".into()), Some(Ok(value.into())));

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
        settle(&ask, Ok("9:00".into()), Some(Ok("4812.0".into())));
        // The catch-up sync, and then a walk that actually counts.
        settle(&ask, Ok("9:00".into()), Some(Ok("4820.0".into())));
        settle(&ask, Ok("9:00".into()), Some(Ok("4840.0".into())));
        assert!(link.walk().unwrap().done());

        // A phone that has not synced, an integration reloading: the walk
        // already counted stands, and the hub is still perfectly reachable.
        for quiet in ["unknown", "unavailable", ""] {
            settle(&ask, Ok("9:00".into()), Some(Ok(quiet.into())));
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
            settle(&ask, Ok("9:00".into()), gone());
            assert_ne!(link.reachable(), Some(false), "one miss is not an outage");
        }
        settle(&ask, Ok("9:00".into()), gone());
        assert_eq!(link.reachable(), Some(false));

        // A tag that keeps answering must not paint over a walk that can never
        // be counted -- the gate still has a half that will not close.
        settle_tag(&ask, Ok("9:01".into()));
        assert_eq!(link.reachable(), Some(false), "the steps are still gone");
        settle(&ask, Ok("9:01".into()), Some(Ok("4812.0".into())));
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

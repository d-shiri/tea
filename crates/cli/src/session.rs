//! Idle time and inhibitors, read from the GNOME session over D-Bus.
//!
//! Everything here degrades rather than fails. A break timer that refuses to
//! run because gnome-shell restarted, or because you are on SSH, is worse than
//! one running on a cruder signal — so a missing service costs accuracy, never
//! availability.

use std::collections::HashMap;
use std::time::Duration;
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedObjectPath, Value};

/// Mutter tracks time since the last input event, which is exactly what we
/// want: it keeps climbing while you are in a meeting, and resets the moment
/// you touch the keyboard.
const IDLE_DEST: &str = "org.gnome.Mutter.IdleMonitor";
const IDLE_PATH: &str = "/org/gnome/Mutter/IdleMonitor/Core";

const SESSION_DEST: &str = "org.gnome.SessionManager";
const SESSION_PATH: &str = "/org/gnome/SessionManager";

/// gnome-session's inhibit flag for "do not mark this session idle" — what
/// video players, screen sharing and presentation mode set.
const INHIBIT_IDLE: u32 = 8;

/// Each inhibitor is its own object, and can say which app registered it.
const INHIBITOR_IFACE: &str = "org.gnome.SessionManager.Inhibitor";

const NOTIFY_DEST: &str = "org.freedesktop.Notifications";
const NOTIFY_PATH: &str = "/org/freedesktop/Notifications";

pub struct Session {
    conn: Option<Connection>,
    idle: Option<Proxy<'static>>,
    inhibit: Option<Proxy<'static>>,
    /// Complain once per failing service, not once per second.
    complained_idle: bool,
    complained_inhibit: bool,
}

impl Session {
    /// Never fails. What it cannot reach, it reports as unavailable.
    pub fn connect() -> Self {
        let conn = match Connection::session() {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "tea: no session bus ({e}); falling back to suspend-gap idle detection"
                );
                return Self {
                    conn: None,
                    idle: None,
                    inhibit: None,
                    complained_idle: true,
                    complained_inhibit: true,
                };
            }
        };

        let idle = Proxy::new(&conn, IDLE_DEST, IDLE_PATH, IDLE_DEST).ok();
        let inhibit = Proxy::new(&conn, SESSION_DEST, SESSION_PATH, SESSION_DEST).ok();
        Self {
            conn: Some(conn),
            idle,
            inhibit,
            complained_idle: false,
            complained_inhibit: false,
        }
    }

    /// Time since the last input event, or `None` if the compositor can't say.
    pub fn idle(&mut self) -> Option<Duration> {
        let proxy = self.idle.as_ref()?;
        match proxy.call::<_, _, u64>("GetIdletime", &()) {
            Ok(ms) => Some(Duration::from_millis(ms)),
            Err(e) => {
                if !self.complained_idle {
                    self.complained_idle = true;
                    eprintln!(
                        "tea: idle monitor unavailable ({e}); \
                         falling back to suspend-gap detection"
                    );
                }
                None
            }
        }
    }

    /// Is something asking not to be interrupted right now?
    pub fn inhibited(&mut self) -> bool {
        let Some(proxy) = self.inhibit.as_ref() else {
            return false;
        };
        match proxy.call::<_, _, bool>("IsInhibited", &(INHIBIT_IDLE,)) {
            Ok(v) => v,
            Err(e) => {
                if !self.complained_inhibit {
                    self.complained_inhibit = true;
                    eprintln!(
                        "tea: inhibitor check unavailable ({e}); \
                         breaks will not be deferred for screen sharing"
                    );
                }
                false
            }
        }
    }
}

impl Session {
    /// Which apps are currently holding the session awake, named as best the
    /// session manager can. Empty when nothing is, or when we cannot ask.
    pub fn inhibitors(&mut self) -> Vec<String> {
        let (Some(conn), Some(proxy)) = (&self.conn, &self.inhibit) else {
            return Vec::new();
        };
        let paths: Vec<OwnedObjectPath> =
            proxy.call("GetInhibitors", &()).unwrap_or_default();

        paths
            .into_iter()
            .filter_map(|path| {
                let item = Proxy::new(conn, SESSION_DEST, path, INHIBITOR_IFACE).ok()?;
                // Only idle inhibitors matter here; a logout inhibitor is
                // not a reason to hold back a break.
                let flags: u32 = item.call("GetFlags", &()).unwrap_or(0);
                if flags & INHIBIT_IDLE == 0 {
                    return None;
                }
                let app: String = item.call("GetAppId", &()).unwrap_or_default();
                let reason: String = item.call("GetReason", &()).unwrap_or_default();
                Some(match (app.trim(), reason.trim()) {
                    ("", "") => "an unnamed app".to_string(),
                    ("", r) => r.to_string(),
                    (a, "") => a.to_string(),
                    (a, r) => format!("{a} ({r})"),
                })
            })
            .collect()
    }

    /// Best-effort desktop notification. Silence is an acceptable outcome --
    /// this is never the only place something gets reported.
    pub fn notify(&mut self, summary: &str, body: &str) {
        let Some(conn) = &self.conn else { return };
        let Ok(proxy) = Proxy::new(conn, NOTIFY_DEST, NOTIFY_PATH, NOTIFY_DEST) else {
            return;
        };
        let _: Result<u32, _> = proxy.call(
            "Notify",
            &(
                "tea",
                0u32,
                "appointment-soon",
                summary,
                body,
                Vec::<String>::new(),
                HashMap::<&str, Value>::new(),
                10_000i32,
            ),
        );
    }
}

//! GTK4 soft-enforcement overlay for GNOME/Wayland.
//!
//! "Soft" is a deliberate limit, not an oversight: Wayland has no equivalent of
//! `XGrabKeyboard`, and Mutter implements no layer-shell protocol, so a normal
//! client cannot take the seat. What it *can* do is cover every monitor, refuse
//! to close, and shove itself back in front when you switch away. Anyone
//! determined can still escape; the goal is to make ignoring it deliberate.

use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use crate::config::Dur;
use crate::session::Session;
use tea_core::{Blocker, Snooze};
use serde::Deserialize;
use std::cell::{Cell, RefCell};
use std::f64::consts::{FRAC_PI_2, TAU};
use std::rc::Rc;
use std::time::Duration;

const CSS: &str = "
window.tea-overlay { background-color: transparent; }
.tea-backdrop { background-color: #0d1017; }
window.tea-toast { background-color: #1b2030; border-radius: 14px; }
window.tea-toast button {
    background-image: none;
    background-color: #2b3450;
    color: #e6e9f0;
    border: 1px solid #3a4560;
    border-radius: 9px;
    padding: 8px 14px;
    font-size: 12pt;
}
window.tea-toast button:hover { background-color: #374260; }
.tea-title { font-size: 26pt; font-weight: 300; color: #e6e9f0; }
.tea-count { font-size: 56pt; font-weight: 200; color: #e6e9f0; }
.tea-sub   { font-size: 12pt; color: #79839c; }
.tea-warn-text { font-size: 15pt; color: #e6e9f0; }
.tea-warn-sub  { font-size: 11pt; color: #79839c; }
";

pub struct GtkBlocker {
    app: gtk::Application,
    /// Raised by the postpone button and consumed by the engine on its next
    /// tick. A GTK callback must never reach into the scheduler directly: it
    /// fires from inside the same main loop that is mid-tick.
    postpone: Rc<Cell<bool>>,
    /// One fullscreen window per monitor. Index 0 is the one that fights for
    /// focus; see `engage`.
    anim: Anim,
    hold: Hold,
    /// One page per monitor. Shared, because insisting replaces them: see
    /// `insist`, which builds a fresh window rather than re-showing a hidden
    /// one, and has to put the new one somewhere `update` will find it.
    pages: Rc<RefCell<Vec<Page>>>,
    warning: Option<gtk::ApplicationWindow>,
    /// True for as long as the current break's windows are meant to be on
    /// screen. The insisting below runs on timers that outlive a single tick,
    /// and a timer that re-shows a window after the break has ended would leave
    /// the screen covered with no way back.
    live: Rc<Cell<bool>>,
    /// Asked whether the user has touched anything lately. Insisting is gated
    /// on input, not on focus: see `insist` for why focus alone lies.
    session: Rc<RefCell<Session>>,
    /// Watches the monitor list while a break is up, so a screen plugged in or
    /// unplugged mid-break gets its page added or dropped rather than either an
    /// uncovered monitor or a window with nowhere to go.
    monitors_watch: Option<(gio::ListModel, glib::SignalHandlerId)>,
}

impl GtkBlocker {
    pub fn new(
        app: &gtk::Application,
        postpone: Rc<Cell<bool>>,
        anim: Anim,
        hold: Hold,
    ) -> Self {
        if let Some(display) = gdk::Display::default() {
            let provider = gtk::CssProvider::new();
            provider.load_from_string(CSS);
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
        Self {
            app: app.clone(),
            postpone,
            anim,
            hold,
            pages: Rc::new(RefCell::new(Vec::new())),
            warning: None,
            live: Rc::new(Cell::new(false)),
            session: Rc::new(RefCell::new(Session::connect())),
            monitors_watch: None,
        }
    }
}

fn monitors_in(list: &gio::ListModel) -> Vec<gdk::Monitor> {
    (0..list.n_items())
        .filter_map(|i| list.item(i).and_then(|o| o.downcast::<gdk::Monitor>().ok()))
        .collect()
}

impl Blocker for GtkBlocker {
    fn engage(&mut self, total: Duration) {
        self.release();
        println!("\n[BREAK] stop. {} of rest.", clock(total));
        self.live = Rc::new(Cell::new(true));

        let Some(display) = gdk::Display::default() else {
            eprintln!("tea: no display — overlay not shown");
            return;
        };
        let monitors = display.monitors();
        let built: Vec<Page> = monitors_in(&monitors)
            .iter()
            .map(|monitor| build_page(&self.app, monitor, total, total, &self.anim, Entrance::Full))
            .collect();

        if built.is_empty() {
            eprintln!("tea: no monitors found — overlay not shown");
            return;
        }

        let soft = !self.hold.insists();
        if soft {
            arm_soft(&built[0]);
        } else {
            insist(
                &self.app,
                Rc::clone(&self.pages),
                total,
                self.anim.clone(),
                self.hold.every(),
                Rc::clone(&self.live),
                Rc::clone(&self.session),
            );
        }

        *self.pages.borrow_mut() = built;

        // Screens come and go mid-break — a laptop docked or undocked. Follow
        // the list: a monitor that appears would otherwise be an uncovered
        // desk, and pages must land on the monitors that exist *now*, not the
        // ones the break started with.
        let app = self.app.clone();
        let pages = Rc::clone(&self.pages);
        let live = Rc::clone(&self.live);
        let anim = self.anim.clone();
        let watch = monitors.connect_items_changed(move |list, _, _, _| {
            if !live.get() {
                return;
            }
            let old: Vec<Page> = pages.borrow().clone();
            let Some(first) = old.first() else {
                return;
            };
            let left = Duration::from_secs_f64(first.dial.borrow().remaining.max(0.0));
            let fresh: Vec<Page> = monitors_in(list)
                .iter()
                .map(|m| build_page(&app, m, total, left, &anim, Entrance::None))
                .collect();
            // Every monitor gone at once (a lid closing, a dock resetting):
            // keep the old pages. They will be rebuilt onto whatever comes
            // back, and dropping them here would end the coverage for good.
            if fresh.is_empty() {
                return;
            }
            if soft {
                arm_soft(&fresh[0]);
            }
            *pages.borrow_mut() = fresh;
            for page in old {
                page.win.destroy();
            }
        });
        self.monitors_watch = Some((monitors.clone(), watch));
    }

    fn update(&mut self, remaining: Duration) {
        let text = clock(remaining);
        for page in self.pages.borrow().iter() {
            page.count.set_text(&text);
            // The dial runs itself between ticks so the sweep is smooth; this
            // is the once-a-second correction back to what the scheduler says.
            page.dial.borrow_mut().remaining = remaining.as_secs_f64();
        }
    }

    fn release(&mut self) {
        // Before anything else: whatever is still insisting must stop insisting
        // now, not on its next tick.
        self.live.set(false);
        if let Some((monitors, watch)) = self.monitors_watch.take() {
            monitors.disconnect(watch);
        }
        let pages: Vec<Page> = self.pages.borrow_mut().drain(..).collect();
        if pages.is_empty() {
            return;
        }
        for page in pages {
            // close_request is wired to Stop, so ask the window to go away in a
            // way it cannot veto.
            page.win.destroy();
        }
        println!("[back]  break over, timer reset.");
    }

    fn warn(&mut self, until_break: Duration, snooze: Option<Snooze>) {
        println!("[warn]  break in {}", clock(until_break));
        self.clear_warning();

        // A window that only announces something is coming is pure nuisance --
        // it steals focus to tell you a thing you can do nothing about. Only
        // open one when there is a button worth pressing; the host sends a
        // notification otherwise.
        let Some(snooze) = snooze else {
            return;
        };

        let win = gtk::ApplicationWindow::builder()
            .application(&self.app)
            .title("tea")
            .decorated(false)
            .resizable(false)
            .build();
        win.add_css_class("tea-toast");
        // A fixed minimum width. Without it the window shrinks as the countdown
        // text gets shorter, and the compositor re-centres it every second --
        // which reads as the window twitching.
        win.set_size_request(360, -1);

        let text = gtk::Label::new(Some(&format!("Break in {}", words(until_break))));
        text.add_css_class("tea-warn-text");

        let button = gtk::Button::with_label(&format!("Give me {} more", words(snooze.duration)));
        let flag = Rc::clone(&self.postpone);
        let win_for_button = win.clone();
        button.connect_clicked(move |_| {
            flag.set(true);
            win_for_button.destroy();
        });

        let left = gtk::Label::new(Some(&match snooze.left {
            1 => "1 postpone left this hour".to_string(),
            n => format!("{n} postpones left this hour"),
        }));
        left.add_css_class("tea-warn-sub");

        let column = gtk::Box::new(gtk::Orientation::Vertical, 10);
        column.set_margin_top(18);
        column.set_margin_bottom(18);
        column.set_margin_start(22);
        column.set_margin_end(22);
        column.append(&text);
        column.append(&button);
        column.append(&left);

        win.set_child(Some(&column));
        win.present();

        // Opacity only. Animating anything that affects layout would resize the
        // window on every frame.
        animate_in(&column, 0.25, 0, 0.0);

        // Count down for real. A number that was true when the window opened
        // and never again is worse than no number at all.
        let remaining = Cell::new(until_break);
        let label = text.clone();
        let watched = win.downgrade();
        glib::timeout_add_seconds_local(1, move || {
            let Some(win) = watched.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let left = remaining.get().saturating_sub(Duration::from_secs(1));
            remaining.set(left);
            if left.is_zero() || !win.is_visible() {
                return glib::ControlFlow::Break;
            }
            label.set_text(&format!("Break in {}", words(left)));
            glib::ControlFlow::Continue
        });

        self.warning = Some(win);
    }

    fn clear_warning(&mut self) {
        if let Some(win) = self.warning.take() {
            win.destroy();
        }
    }

    fn note(&mut self, msg: &str) {
        println!("[note]  {msg}");
    }
}

/// Terminal fallback: `--headless`, and the only thing that works over SSH.
pub struct TerminalBlocker;

impl Blocker for TerminalBlocker {
    fn engage(&mut self, total: Duration) {
        println!("\n[BREAK] stop. {} of rest.", clock(total));
    }

    fn update(&mut self, remaining: Duration) {
        use std::io::Write;
        print!("\r[BREAK] {} remaining   ", clock(remaining));
        let _ = std::io::stdout().flush();
    }

    fn release(&mut self) {
        println!("\r[back]  break over, timer reset.      ");
    }

    fn warn(&mut self, until_break: Duration, snooze: Option<Snooze>) {
        match snooze {
            Some(s) => println!("[warn]  break in {} ({} postpones left)", clock(until_break), s.left),
            None => println!("[warn]  break in {} (no postpones left)", clock(until_break)),
        }
    }

    fn note(&mut self, msg: &str) {
        println!("[note]  {msg}");
    }
}

/// What the break page does when you switch away from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Grip {
    /// It covers the screen and asks once for the focus. Alt-Tab away and it
    /// stays where it is, behind whatever you switched to.
    #[default]
    Soft,
    /// It puts itself back in front, for as long as the break lasts. Leaving is
    /// still physically possible -- see `insist` -- but you have to keep doing
    /// it, which is the point.
    Insist,
}

/// How hard the break page fights to stay in front.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Hold {
    pub mode: Grip,
    /// How often an insisting page checks whether it is still the one in front.
    pub recheck: Dur,
}

impl Default for Hold {
    fn default() -> Self {
        // Soft by default. A tool that seizes the screen the first time you run
        // it, before you have agreed to that, is a tool you uninstall.
        Self { mode: Grip::Soft, recheck: Dur(Duration::from_millis(400)) }
    }
}

impl Hold {
    fn insists(&self) -> bool {
        self.mode == Grip::Insist
    }

    /// Clamped: fast enough to beat a deliberate switch, slow enough that a
    /// typo cannot turn the poll into a busy loop.
    fn every(&self) -> Duration {
        self.recheck.0.clamp(Duration::from_millis(100), Duration::from_secs(5))
    }
}

/// One screen's worth of break page: the window, the screen it belongs to, and
/// the two things that have to be kept up to date while the break runs.
///
/// Cloning one shares the same window and dial rather than copying them, which
/// is what lets a page be handed to a timer without freezing what it shows.
#[derive(Clone)]
struct Page {
    win: gtk::ApplicationWindow,
    monitor: gdk::Monitor,
    count: gtk::Label,
    dial: Rc<RefCell<Dial>>,
}

/// Soft mode's one concession: a single window asks once for the focus back,
/// and takes no for an answer. Only one page gets this, because a request from
/// every screen at once is several windows stealing the focus from each other.
fn arm_soft(page: &Page) {
    page.win.connect_is_active_notify(|w| {
        if !w.is_active() && w.is_visible() {
            w.present();
        }
    });
}

/// Whether a page plays the arrival animation.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Entrance {
    /// The break has just started: the dark washes in, the blast goes out, the
    /// dial lands.
    Full,
    /// A page being put back in front mid-break. It has to be *there*, at once,
    /// showing the time that is actually left. Replaying the explosion every
    /// time you switched away would be pantomime, and would hide the countdown
    /// behind three seconds of animation each time.
    None,
}

/// Build one page and put it on screen.
///
/// `remaining` is separate from `total` because a page is not always born at
/// the start of a break: `insist` builds replacements part-way through, and
/// they have to arrive showing the right time on the clock and the right amount
/// of ring left.
fn build_page(
    app: &gtk::Application,
    monitor: &gdk::Monitor,
    total: Duration,
    remaining: Duration,
    anim: &Anim,
    entrance: Entrance,
) -> Page {
    let arrival = match entrance {
        Entrance::Full => anim.seconds(),
        Entrance::None => 0.0,
    };

    let win = gtk::ApplicationWindow::builder()
        .application(app)
        .decorated(false)
        .title("tea")
        .build();
    win.add_css_class("tea-overlay");

    // One drawing surface covering the whole window. It has to be the
    // full page: cairo clips to the widget, so a blast drawn inside a
    // small dial can never reach the edges of the screen.
    let (stage, state) = build_stage(total, remaining, arrival, anim);

    // Sized from the screen, not fixed: a slot generous enough to clear
    // the dial on a large display would not fit on a laptop panel at
    // all, and the column would be clipped.
    let geometry = monitor.geometry();
    let slot = slot_height(geometry.width(), geometry.height());
    let mark = logo_height(ring_radius(geometry.width(), geometry.height()));

    // The countdown must land on the exact centre of the screen, where
    // the dial is drawn. Rather than offsetting the other two from the
    // centre -- which left the subtitle sitting on top of the numbers --
    // the title and subtitle are given equal fixed heights above and
    // below. Equal slots put the middle child in the middle by
    // construction, whatever the text in them turns out to be.
    let count = gtk::Label::new(Some(&clock(remaining)));
    count.add_css_class("tea-count");

    let title = gtk::Label::new(Some("Time to stop"));
    title.add_css_class("tea-title");

    // The mark and the title share the upper slot. They are grouped and
    // centred inside it rather than packed from its top edge, so they
    // stay balanced against the subtitle below.
    let group = gtk::Box::new(gtk::Orientation::Vertical, 16);
    group.set_halign(gtk::Align::Center);
    group.set_valign(gtk::Align::Center);
    group.set_vexpand(true);
    if let Some(mark) = logo(mark) {
        group.append(&mark);
    }
    group.append(&title);

    let head = gtk::Box::new(gtk::Orientation::Vertical, 0);
    head.set_size_request(-1, slot);
    head.append(&group);

    let sub = gtk::Label::new(Some("Look away from the screen. Stand up."));
    sub.add_css_class("tea-sub");
    sub.set_size_request(-1, slot);

    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.append(&head);
    column.append(&count);
    column.append(&sub);
    column.set_halign(gtk::Align::Center);
    column.set_valign(gtk::Align::Center);

    let layers = gtk::Overlay::new();
    layers.set_child(Some(&stage));
    layers.add_overlay(&column);
    win.set_child(Some(&layers));

    // The words arrive after the blast has passed over them. Every
    // timing is a share of the configured entrance, so turning that one
    // number changes the whole sequence in proportion.
    //
    // Fades only, no sliding: these three share a box, so animating a
    // margin would resize it every frame and the centred column -- the
    // countdown with it -- would twitch for the whole entrance.
    animate_in(&count, arrival * 0.34, 0, arrival * 0.30);
    animate_in(&head, arrival * 0.34, 0, arrival * 0.46);
    animate_in(&sub, arrival * 0.34, 0, arrival * 0.58);

    // Insurance: if the frame clock never delivers -- a stalled
    // compositor, a machine thrashing on resume -- an overlay stuck
    // part-way through would be an invisible break. Force the finished
    // state once the animation has had more than long enough.
    let settled: Vec<gtk::Widget> =
        vec![count.clone().upcast(), head.clone().upcast(), sub.clone().upcast()];
    let finish = Rc::clone(&state);
    let redraw = stage.clone();
    glib::timeout_add_local_once(Duration::from_secs_f64((arrival * 1.6).max(1.0)), move || {
        for w in &settled {
            w.set_opacity(1.0);
        }
        finish.borrow_mut().entrance = 1.0;
        redraw.queue_draw();
    });

    // Escape, Alt-F4 and the rest go nowhere. The break ends when the
    // break ends.
    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed(|_, _, _, _| glib::Propagation::Stop);
    win.add_controller(keys);
    win.connect_close_request(|_| glib::Propagation::Stop);

    // Fullscreen before the window is ever shown. Asking afterwards costs a
    // second round trip with the compositor, and the window spends it at the
    // wrong size.
    win.fullscreen_on_monitor(monitor);
    win.present();

    Page { win, monitor: monitor.clone(), count, dial: state }
}

/// Keep putting the pages back in front until the break is over.
///
/// Wayland has no keyboard grab and Mutter has no layer-shell, so there is no
/// call that says "this window owns the screen now". What is left is attrition.
/// Two levers, in order of rudeness:
///
/// 1. `present`, which is a *request*. GNOME turns it down when the window
///    asking is not one you just interacted with: it flags the window for
///    attention and leaves the focus where it was. That is precisely our case,
///    so on its own it loses.
/// 2. Building the page again, as a brand new window, and destroying the old
///    one behind it. A window that has just appeared is a window the compositor
///    will raise, because it cannot tell that one from any other new window.
///
/// The second lever is a *replacement*, not a re-show, and that is the whole
/// point. Hiding a fullscreen window and showing it again is the obvious way to
/// look new, and it works for about a second: the surface comes back mapped and
/// the right size, but its frame clock never resumes, so nothing is ever drawn
/// into it again. What you get is a page that is unmistakably there and totally
/// black -- the worst of both, since it covers the screen without telling you
/// how long is left. A new window has a new frame clock and no such history.
///
/// Every screen is put back, not just the one with the focus on it. Only one
/// window can be active at a time, but the others can still be buried: raise a
/// window on the second monitor and the page there stays underneath it, which
/// is a break page you can simply work beside. The windows do not fight each
/// other over this, because the question asked each time is about all of them
/// at once -- is *any* of our pages the active window? Clicking the page on the
/// second screen is not an escape, and must not provoke the first screen into
/// snatching the focus back.
///
/// What pulls either lever is the user's *input*, never the focus alone. Focus
/// is a liar here: Mutter refuses it to any window the user has not touched, so
/// a page can be covering every screen, doing its job perfectly, and still not
/// be the active window -- true of every page this daemon has ever presented,
/// since nobody clicks a break page into being. A loop keyed on focus alone
/// tears those perfectly fine pages down and rebuilds them a few times a
/// second for the entire break: a strobe, which is how this function earned
/// its rewrite. What actually marks an escape is input landing somewhere else:
/// typing or mousing while no page has the focus is work happening beside the
/// break. Hands off the keyboard, and the pages are left completely alone,
/// whoever the compositor thinks is active. If the idle monitor cannot be
/// asked at all, insisting degrades to sitting still rather than to strobing.
///
/// Nothing here can stop someone who keeps switching away. It makes escaping a
/// thing you have to keep choosing, which is all a break tool should ever do.
fn insist(
    app: &gtk::Application,
    pages: Rc<RefCell<Vec<Page>>>,
    total: Duration,
    anim: Anim,
    every: Duration,
    live: Rc<Cell<bool>>,
    session: Rc<RefCell<Session>>,
) {
    /// Polite requests ignored before the pages are built again.
    const PATIENCE: u32 = 3;

    /// Input younger than this means the user is working right now. Old enough
    /// that held-down typing never slips through a gap between keystrokes, and
    /// young enough that a user who stops fighting goes quiet before the next
    /// escalation -- at the default cadence the rebuild fires after
    /// `PATIENCE + 1` ticks (1.6s), by which time untouched input is stale.
    const RECENT: Duration = Duration::from_millis(1200);

    let app = app.clone();
    let refused = Cell::new(0u32);

    glib::timeout_add_local(every, move || {
        // The break ended: stop, and never touch the windows again. They are
        // being destroyed, and building a replacement here would leave a page
        // on screen that nothing owns.
        if !live.get() {
            return glib::ControlFlow::Break;
        }
        let showing: Vec<Page> = pages.borrow().clone();
        if showing.is_empty() {
            return glib::ControlFlow::Break;
        }

        if showing.iter().any(|p| p.win.is_active()) {
            refused.set(0);
            return glib::ControlFlow::Continue;
        }

        // Nobody focused on us proves nothing by itself -- see above. Only
        // fresh input somewhere that is not a break page is an escape.
        let working =
            session.borrow_mut().idle().is_some_and(|idle| idle < RECENT);
        if !working {
            refused.set(0);
            return glib::ControlFlow::Continue;
        }

        refused.set(refused.get() + 1);
        if refused.get() <= PATIENCE {
            for page in &showing {
                page.win.present();
            }
            return glib::ControlFlow::Continue;
        }

        // Asked nicely, got nowhere. Come back as new windows, carrying the
        // time that is actually left rather than restarting the break.
        println!("[hold]  still working — the page comes back");
        refused.set(0);
        let fresh: Vec<Page> = showing
            .iter()
            .map(|page| {
                let left = Duration::from_secs_f64(page.dial.borrow().remaining.max(0.0));
                build_page(&app, &page.monitor, total, left, &anim, Entrance::None)
            })
            .collect();

        // The new pages are up before the old ones go, so the screen is never
        // uncovered -- not even for the frame it takes to swap them.
        *pages.borrow_mut() = fresh;
        for page in showing {
            page.win.destroy();
        }
        glib::ControlFlow::Continue
    });
}

/// How the break page arrives. Tunable because taste in this varies more than
/// any other setting here, and because tuning it by rebuilding is miserable.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Anim {
    pub entrance: Dur,
    pub burst: f64,
    pub shards: u32,
}

impl Default for Anim {
    fn default() -> Self {
        Self { entrance: Dur(Duration::from_secs(3)), burst: 0.62, shards: 26 }
    }
}

impl Anim {
    /// Seconds, and never negative or absurd: a config typo should make the
    /// animation odd, not wedge the overlay.
    fn seconds(&self) -> f64 {
        self.entrance.0.as_secs_f64().clamp(0.0, 10.0)
    }

    /// What share of the entrance the debris is visible for. Kept below 1 so
    /// the blast always finishes before the dial settles.
    fn burst_share(&self) -> f64 {
        self.burst.clamp(0.0, 0.95)
    }

    fn shard_count(&self) -> u32 {
        self.shards.min(400)
    }
}

/// What the dial is showing. The clock ticks once a second; this runs on the
/// frame clock in between so the sweep does not stutter.
pub struct Dial {
    total: f64,
    remaining: f64,
    /// 0 to 1 while it arrives, then stays at 1.
    entrance: f64,
    last_frame: i64,
}

/// The mark shown on the break page. Embedded rather than read from disk: it is
/// part of the application, and a logo loaded by path is a logo that eventually
/// goes missing on someone else's machine.
const LOGO: &[u8] = include_bytes!("../../../assets/tea.png");

fn logo(height: i32) -> Option<gtk::Image> {
    let texture = gtk::gdk::Texture::from_bytes(&glib::Bytes::from_static(LOGO)).ok()?;
    // Image, not Picture: `set_size_request` on a Picture is only a *minimum*,
    // so it kept its natural 512px and shoved the countdown off centre.
    // `set_pixel_size` is an exact instruction.
    let image = gtk::Image::from_paintable(Some(&texture));
    image.set_pixel_size(height);
    Some(image)
}

/// A mark, not a poster. Sized from the ring rather than from the slot, because
/// the slot is sized from this -- and something has to break the circle.
fn logo_height(radius: f64) -> i32 {
    (radius * 0.55).clamp(48.0, 96.0) as i32
}

/// The dial's radius. The single number the whole layout is built from -- the
/// text slots are derived from it, so the two cannot disagree about where the
/// ring ends and the words begin.
fn ring_radius(width: i32, height: i32) -> f64 {
    (width.min(height) as f64 * 0.13).clamp(100.0, 180.0)
}

/// Clear air between the ring and the nearest line of text.
const RING_GAP: f64 = 56.0;

/// Rough heights of the three lines. Only estimates -- they decide how much air
/// there is, never whether anything collides.
const COUNT_HEIGHT: f64 = 110.0;
const TITLE_HEIGHT: f64 = 50.0;
const LOGO_GAP: f64 = 16.0;

/// The mark and the title stacked together, which is what the upper slot has to
/// hold. The title sits at the *bottom* of that stack, so it reaches this much
/// further towards the ring than the middle of the slot -- the thing the first
/// version of this formula forgot.
fn head_height(radius: f64) -> f64 {
    logo_height(radius) as f64 + LOGO_GAP + TITLE_HEIGHT
}

/// Height reserved above and below the countdown, for the title and subtitle.
/// Equal by definition, which is what keeps the countdown centred, and derived
/// from the ring so the text always lands outside it.
///
/// Capped so the column can never be taller than the screen: GTK would squeeze
/// the slots to fit, dragging the text back inside the ring -- which is exactly
/// how it went wrong before.
fn slot_height(width: i32, height: i32) -> i32 {
    let radius = ring_radius(width, height);
    // Deep enough that the *bottom of the stack* clears the ring, not merely
    // the middle of the slot.
    let clear_of_ring = 2.0 * (radius + RING_GAP) + head_height(radius) - COUNT_HEIGHT;
    let fits_on_screen = (height as f64 - COUNT_HEIGHT) / 2.0;
    clear_of_ring.min(fits_on_screen).max(150.0) as i32
}

/// The whole page: the dark, a blast that sweeps out past the corners, and the
/// countdown dial settling in the middle of it.
///
/// `arrival` is passed in rather than read from `anim`, because a page built
/// part-way through a break has to skip the entrance whatever the config says.
fn build_stage(
    total: Duration,
    remaining: Duration,
    arrival: f64,
    anim: &Anim,
) -> (gtk::DrawingArea, Rc<RefCell<Dial>>) {
    let burst_share = anim.burst_share();
    let shards = anim.shard_count();

    let state = Rc::new(RefCell::new(Dial {
        total: total.as_secs_f64().max(1.0),
        remaining: remaining.as_secs_f64(),
        // With the animation switched off, start already arrived.
        entrance: if arrival <= 0.0 { 1.0 } else { 0.0 },
        last_frame: 0,
    }));

    let area = gtk::DrawingArea::new();
    area.set_hexpand(true);
    area.set_vexpand(true);

    let drawing = Rc::clone(&state);
    area.set_draw_func(move |_, cr, width, height| {
        let dial = drawing.borrow();
        let entrance = dial.entrance;
        let (cx, cy) = (width as f64 / 2.0, height as f64 / 2.0);
        // Corner to corner, and a little past, so the blast leaves the page
        // rather than stopping short of it.
        let reach = (cx * cx + cy * cy).sqrt() * 1.12;

        // 1. The dark washes in first.
        cr.set_source_rgba(0.051, 0.063, 0.090, phase(entrance, 0.0, 0.18));
        let _ = cr.paint();

        cr.set_line_cap(gtk::cairo::LineCap::Round);

        // 2. The blast, all the way out. Three waves rather than one: a single
        //    expanding circle reads as a ripple, three read as a shock.
        let blast = phase(entrance, 0.02, burst_share.max(0.05));
        if burst_share > 0.0 && blast < 1.0 {
            for (start, weight, thickness) in [(0.0, 0.85, 10.0), (0.16, 0.55, 7.0), (0.34, 0.35, 5.0)] {
                let wave = phase(blast, start, 1.0);
                if wave <= 0.0 {
                    continue;
                }
                cr.set_line_width((thickness * (1.0 - wave)).max(0.5));
                cr.set_source_rgba(0.55, 0.70, 1.0, (1.0 - wave).powi(2) * weight);
                cr.arc(cx, cy, (reach * ease_out_cubic(wave)).max(1.0), 0.0, TAU);
                let _ = cr.stroke();
            }

            let flown = ease_out_cubic(blast);
            let fade = (1.0 - blast).powi(2);
            for i in 0..shards {
                // Varied without randomness: the same break looks the same
                // twice, and nothing here needs a seed.
                let angle = i as f64 / shards.max(1) as f64 * TAU + 0.4;
                let speed = 0.55 + ((i * 7) % 5) as f64 * 0.16;
                let distance = reach * flown * speed;
                cr.set_source_rgba(0.62, 0.74, 1.0, fade);
                cr.arc(
                    cx + angle.cos() * distance,
                    cy + angle.sin() * distance,
                    1.5 + 6.0 * (1.0 - blast),
                    0.0,
                    TAU,
                );
                let _ = cr.fill();
            }
        }

        // 3. The dial arrives in the middle, once the blast is on its way out.
        let arriving = phase(entrance, 0.22, 0.80);
        if arriving <= 0.0 {
            return;
        }
        // Overshoot slightly at the end, so it lands rather than stops.
        let radius = ring_radius(width, height) * (0.25 + 0.75 * ease_out_back(arriving));

        cr.set_line_width(12.0);
        cr.set_source_rgba(0.13, 0.16, 0.24, arriving);
        cr.arc(cx, cy, radius, 0.0, TAU);
        let _ = cr.stroke();

        // Time left, draining clockwise from the top. While it arrives, the arc
        // draws itself around rather than snapping to full.
        let left = (dial.remaining / dial.total).clamp(0.0, 1.0);
        let drawn = left * ease_out_cubic(phase(entrance, 0.42, 1.0));
        if drawn > 0.0 {
            cr.set_source_rgba(0.48, 0.63, 0.97, arriving);
            cr.arc(cx, cy, radius, -FRAC_PI_2, -FRAC_PI_2 + TAU * drawn);
            let _ = cr.stroke();
        }
    });

    let ticking = Rc::clone(&state);
    area.add_tick_callback(move |area, clock| {
        let now = clock.frame_time();
        {
            let mut dial = ticking.borrow_mut();
            let delta = if dial.last_frame == 0 {
                0.0
            } else {
                (now - dial.last_frame) as f64 / 1_000_000.0
            };
            dial.last_frame = now;
            dial.entrance = if arrival <= 0.0 {
                1.0
            } else {
                (dial.entrance + delta / arrival).min(1.0)
            };
            dial.remaining = (dial.remaining - delta).max(0.0);
        }
        area.queue_draw();
        glib::ControlFlow::Continue
    });

    (area, state)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The recheck interval drives a timer on the main loop. Zero would be a
    /// busy loop that fights the compositor at frame rate, and a value in hours
    /// would mean the page never actually insists.
    #[test]
    fn the_recheck_interval_cannot_be_absurd() {
        let at = |d| Hold { mode: Grip::Insist, recheck: Dur(d) }.every();

        assert_eq!(at(Duration::ZERO), Duration::from_millis(100));
        assert_eq!(at(Duration::from_secs(3600)), Duration::from_secs(5));
        assert_eq!(at(Duration::from_millis(250)), Duration::from_millis(250));
        assert_eq!(Hold::default().every(), Duration::from_millis(400));
    }

    #[test]
    fn soft_is_the_default_and_does_not_insist() {
        assert!(!Hold::default().insists());
        assert!(Hold { mode: Grip::Insist, ..Hold::default() }.insists());
    }

    /// The layout has one job: the words go outside the ring, on every screen.
    #[test]
    fn the_text_always_clears_the_dial() {
        const SUB_HEIGHT: f64 = 40.0;

        for (w, h) in [(1280, 720), (1366, 768), (1920, 1080), (2560, 1440), (3840, 2160)] {
            let slot = slot_height(w, h) as f64;
            let radius = ring_radius(w, h);

            // What matters is the edge of the text nearest the ring, not the
            // middle of the slot it sits in. The title is the bottom of the
            // mark-and-title stack; the subtitle is centred in its own slot.
            let title_bottom = COUNT_HEIGHT / 2.0 + slot / 2.0 - head_height(radius) / 2.0;
            let sub_top = COUNT_HEIGHT / 2.0 + slot / 2.0 - SUB_HEIGHT / 2.0;

            // On a short screen the slot gets capped and the air narrows, but
            // it must never run out.
            for (what, edge) in [("title", title_bottom), ("subtitle", sub_top)] {
                assert!(
                    edge >= radius + 24.0,
                    "{w}x{h}: {what} reaches {edge:.0}px from centre, ring reaches {radius:.0}px"
                );
            }

            // If the column outgrows the screen, GTK squeezes the slots and the
            // text lands back on the ring.
            assert!(
                slot * 2.0 + COUNT_HEIGHT <= h as f64,
                "{w}x{h}: column is {:.0}px on a {h}px screen",
                slot * 2.0 + COUNT_HEIGHT
            );
        }
    }
}

/// Progress through one slice of a longer sequence, clamped to its own 0..1.
fn phase(progress: f64, start: f64, end: f64) -> f64 {
    if end <= start {
        return if progress >= end { 1.0 } else { 0.0 };
    }
    ((progress - start) / (end - start)).clamp(0.0, 1.0)
}

fn ease_out_cubic(t: f64) -> f64 {
    1.0 - (1.0 - t).powi(3)
}

/// Ease out with a little overshoot at the end.
fn ease_out_back(t: f64) -> f64 {
    const C: f64 = 1.70158;
    let t = t - 1.0;
    1.0 + (C + 1.0) * t.powi(3) + C * t.powi(2)
}

/// Fade a widget in, optionally sliding it up `slide` pixels as it goes.
///
/// Driven by the frame clock rather than a timer, so it stays smooth when the
/// machine is busy and costs nothing when it is not being drawn.
fn animate_in(widget: &impl IsA<gtk::Widget>, seconds: f64, slide: i32, delay: f64) {
    let widget = widget.as_ref().clone();
    // Switched off: arrive already arrived, with no frame of opacity 0 first.
    if seconds <= 0.0 {
        widget.set_opacity(1.0);
        return;
    }
    widget.set_opacity(0.0);
    if slide != 0 {
        widget.set_margin_top(slide);
    }

    let started = Cell::new(0i64);
    widget.add_tick_callback(move |w, clock| {
        let now = clock.frame_time();
        if started.get() == 0 {
            started.set(now);
        }
        let elapsed = ((now - started.get()) as f64) / 1_000_000.0 - delay;
        let progress = (elapsed / seconds).clamp(0.0, 1.0);
        // Ease out: quick to appear, gentle to settle.
        let eased = 1.0 - (1.0 - progress).powi(3);

        w.set_opacity(eased);
        if slide != 0 {
            w.set_margin_top((slide as f64 * (1.0 - eased)).round() as i32);
        }

        if progress >= 1.0 { glib::ControlFlow::Break } else { glib::ControlFlow::Continue }
    });
}

/// Durations in words, for anything a person reads under pressure. `0:30` and
/// `1:00` sitting next to each other look like the same kind of number when
/// one is a countdown and the other is an amount of delay.
fn words(d: Duration) -> String {
    let secs = d.as_secs();
    match (secs / 60, secs % 60) {
        (0, 1) => "1 second".to_string(),
        (0, s) => format!("{s} seconds"),
        (1, 0) => "1 minute".to_string(),
        (m, 0) => format!("{m} minutes"),
        (m, s) => format!("{m}m {s}s"),
    }
}

fn clock(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

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
.tea-await { font-size: 30pt; font-weight: 300; color: #e6e9f0; }
.tea-sub   { font-size: 12pt; color: #79839c; }
.tea-tag       { font-size: 11pt; color: #79839c; }
.tea-tag.done  { color: #73d19e; }
.tea-tag.unseen { color: #c9a35f; }
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
    /// What to ask for once the countdown has run out, if anything. Shared,
    /// because a page rebuilt mid-wait -- by `insist`, or by a monitor being
    /// plugged in -- has to be born already asking. A page that came back
    /// showing a fresh countdown would be telling you a lie about a break that
    /// is over.
    ask: Option<String>,
    waiting: Rc<Cell<bool>>,
    /// Whether the tag has been scanned for the break on screen. Shared for the
    /// same reason `waiting` is: a page rebuilt by `insist` has to come back
    /// knowing it, or every escape attempt would quietly undo the walk you
    /// already made.
    scanned: Rc<Cell<bool>>,
    /// Whether a scan could be noticed at all. Shared for the same reason as
    /// the two above: a rebuilt page must not go back to promising something
    /// that cannot happen.
    reachable: Rc<Cell<bool>>,
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
        ask: Option<String>,
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
            ask,
            waiting: Rc::new(Cell::new(false)),
            scanned: Rc::new(Cell::new(false)),
            reachable: Rc::new(Cell::new(true)),
            live: Rc::new(Cell::new(false)),
            session: Rc::new(RefCell::new(Session::connect())),
            monitors_watch: None,
        }
    }
}

/// Everything a page shows that is not the clock.
#[derive(Clone, Default)]
struct Face {
    /// Waiting, and the scan has landed: the page is a second from coming down
    /// and should say so rather than still asking. Rebuilt pages included --
    /// `insist` can replace one between the scan and the page lifting.
    done: bool,
    /// Set once the countdown is spent and the page is only waiting: what to
    /// ask for, in the user's own words.
    ask: Option<String>,
    /// `None` when nothing is gating this break, and the page carries no badge
    /// at all -- a break that ends by itself has nothing to report.
    tag: Option<Tag>,
}

/// Whether the walk has been made yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tag {
    Pending,
    Scanned,
    /// Nothing is watching for a scan that could be reached. Worth saying on
    /// the page rather than in a log: the alternative is standing in another
    /// room waving a phone at a sticker that was never going to work.
    Unseen,
}

impl GtkBlocker {
    /// A closure the page-rebuilding paths can ask, at the moment they rebuild,
    /// what the new page should show. Passing the answer itself would freeze it
    /// as it was when the break started -- and a page that came back saying the
    /// tag was still unscanned would be asking for a second walk.
    fn face(&self) -> impl Fn() -> Face + 'static {
        let waiting = Rc::clone(&self.waiting);
        let scanned = Rc::clone(&self.scanned);
        let reachable = Rc::clone(&self.reachable);
        let ask = self.ask.clone();
        move || face_of(ask.as_deref(), waiting.get(), scanned.get(), reachable.get())
    }
}

/// Split out from the closure above so the rules can be checked without a
/// display: what a page shows is decided here, and a rebuilt page that forgot
/// a scan would send someone back down the hall for nothing.
fn face_of(ask: Option<&str>, waiting: bool, scanned: bool, reachable: bool) -> Face {
    Face {
        done: waiting && scanned,
        // Only once the countdown is spent. Before that the page has a clock to
        // show, and the badge below says all that needs saying about the tag.
        ask: ask.filter(|_| waiting).map(str::to_string),
        // The prompt doubles as the switch: it is set exactly when a tag is
        // what ends this break.
        tag: ask.map(|_| match (scanned, reachable) {
            // A walk already made outlives the hub it was reported through:
            // once the scan is in, it does not matter that the thing which saw
            // it has since gone away.
            (true, _) => Tag::Scanned,
            (false, false) => Tag::Unseen,
            (false, true) => Tag::Pending,
        }),
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
        // A break that is resumed mid-wait engages and is told to ask again in
        // the same tick, so these only clear what the last break left behind.
        self.waiting.set(false);
        self.scanned.set(false);
        self.reachable.set(true);

        let Some(display) = gdk::Display::default() else {
            eprintln!("tea: no display — overlay not shown");
            return;
        };
        // Built once and shared by every screen: they are all showing the same
        // break, and a fresh one at that -- nothing has been scanned for it yet.
        let face = self.face()();
        let monitors = display.monitors();
        let built: Vec<Page> = monitors_in(&monitors)
            .iter()
            .map(|monitor| {
                build_page(&self.app, monitor, total, total, &self.anim, Entrance::Full, &face)
            })
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
                self.face(),
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
        let facing = self.face();
        let watch = monitors.connect_items_changed(move |list, _, _, _| {
            if !live.get() {
                return;
            }
            let old: Vec<Page> = pages.borrow().clone();
            let Some(first) = old.first() else {
                return;
            };
            let left = Duration::from_secs_f64(first.dial.borrow().remaining.max(0.0));
            let face = facing();
            let fresh: Vec<Page> = monitors_in(list)
                .iter()
                .map(|m| build_page(&app, m, total, left, &anim, Entrance::None, &face))
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
        if self.waiting.get() {
            return;
        }
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
        self.waiting.set(false);
        if let Some((monitors, watch)) = self.monitors_watch.take() {
            monitors.disconnect(watch);
        }
        let pages: Vec<Page> = self.pages.borrow_mut().drain(..).collect();
        if pages.is_empty() {
            return;
        }
        // A celebration still playing gets to finish: the pages hang on for
        // whatever is left of it and then go. A scan is the one way a break
        // ends that the user personally earned, and tearing the page down
        // mid-confetti hands them a desk instead of a reward. Insisting has
        // already stopped -- `live` is down -- so the linger is only a linger.
        let linger = pages
            .iter()
            .filter_map(|p| p.dial.borrow().celebrate)
            .map(|t| CELEBRATE * (1.0 - t))
            .fold(0.0, f64::max);
        if linger > 0.05 {
            glib::timeout_add_local_once(Duration::from_secs_f64(linger), move || {
                for page in &pages {
                    page.win.destroy();
                }
            });
        } else {
            for page in pages {
                // close_request is wired to Stop, so ask the window to go away
                // in a way it cannot veto.
                page.win.destroy();
            }
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

    fn release_source(&mut self, reachable: bool) {
        // Told every tick, so it must do nothing at all when nothing changed.
        if self.reachable.get() == reachable || self.scanned.get() {
            return;
        }
        self.reachable.set(reachable);
        if !reachable {
            println!("[wait]  nothing is watching for a scan — saying so on the page");
        }
        let tag = if reachable { Tag::Pending } else { Tag::Unseen };
        for page in self.pages.borrow().iter() {
            if let Some(badge) = &page.badge {
                badge.paint(tag);
                animate_in(&badge.row, 0.25, 0, 0.0);
            }
        }
    }

    fn release_seen(&mut self) {
        // Called on every tick the scheduler still has a scan banked, so that a
        // page resumed after a restart comes back green. Doing the work once is
        // the point: repainting a label every second is how a page ends up
        // flickering at someone who is trying to rest.
        if self.scanned.replace(true) {
            return;
        }
        let waiting = self.waiting.get();
        for page in self.pages.borrow().iter() {
            if waiting {
                thank_them(page);
            }
            // The walk happened: confetti, on every screen at once. A page
            // that was only waiting for this stays up long enough to play it
            // -- see `release`.
            page.dial.borrow_mut().celebrate = Some(0.0);
            if let Some(badge) = &page.badge {
                badge.paint(Tag::Scanned);
                // A quarter of a second of fade, so the change registers as
                // something that just happened rather than something that was
                // always there.
                animate_in(&badge.row, 0.25, 0, 0.0);
            }
        }
    }

    fn await_release(&mut self) {
        let Some(ask) = self.ask.clone() else {
            // Nothing is gating this break, so there is nothing to ask for and
            // the page is about to come down anyway.
            return;
        };
        println!("[wait]  time served — {ask}");
        self.waiting.set(true);
        for page in self.pages.borrow().iter() {
            ask_for_the_tag(page, &ask);
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

    fn await_release(&mut self) {
        println!("\r[wait]  time served — waiting for the tag.");
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
    title: gtk::Label,
    sub: gtk::Label,
    /// Absent unless a tag is what ends this break.
    badge: Option<Badge>,
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

/// The "have you been yet?" line at the foot of the page.
///
/// It exists because the break page is otherwise silent about the one thing you
/// have to do to end it. Scanning a tag gives you no feedback at the laptop --
/// the phone says it worked, the page said nothing, and the natural response to
/// that is to walk back and scan it again.
#[derive(Clone)]
struct Badge {
    row: gtk::Box,
    icon: gtk::DrawingArea,
    label: gtk::Label,
    state: Rc<Cell<Tag>>,
}

impl Badge {
    fn new(tag: Tag) -> Self {
        let state = Rc::new(Cell::new(tag));

        let icon = gtk::DrawingArea::new();
        icon.set_content_width(BADGE);
        icon.set_content_height(BADGE);
        icon.set_valign(gtk::Align::Center);
        let drawing = Rc::clone(&state);
        icon.set_draw_func(move |_, cr, width, height| {
            draw_tag(cr, width, height, drawing.get());
        });

        let label = gtk::Label::new(Some(words_for(tag)));
        label.add_css_class("tea-tag");

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 9);
        row.set_halign(gtk::Align::Center);
        row.append(&icon);
        row.append(&label);

        let badge = Self { row, icon, label, state };
        badge.paint(tag);
        badge
    }

    fn paint(&self, tag: Tag) {
        self.state.set(tag);
        self.label.set_text(words_for(tag));
        // One class, toggled, rather than two that could both be on: a label
        // that is somehow "not scanned" and green is worse than no badge.
        for (class, on) in
            [("done", tag == Tag::Scanned), ("unseen", tag == Tag::Unseen)]
        {
            if on {
                self.label.add_css_class(class);
            } else {
                self.label.remove_css_class(class);
            }
        }
        self.icon.queue_draw();
    }
}

fn words_for(tag: Tag) -> &'static str {
    match tag {
        Tag::Pending => "Tag not scanned yet",
        Tag::Scanned => "Tag scanned",
        Tag::Unseen => "Can't see the tag — this break ends on the clock",
    }
}

/// How tall the badge's glyph is drawn. Fixed, like the type beside it: this is
/// a footnote, and a footnote that scales with the screen stops being one.
const BADGE: i32 = 22;

/// Two glyphs, drawn rather than pulled from an icon theme -- the same reason
/// the mark on this page is embedded. An icon that is present on the machine
/// you built on and missing on someone else's is a blank square in the middle
/// of the one screen they cannot dismiss.
fn draw_tag(cr: &gtk::cairo::Context, width: i32, height: i32, tag: Tag) {
    let (cx, cy) = (width as f64 / 2.0, height as f64 / 2.0);
    let r = (width.min(height) as f64) / 2.0 - 1.5;
    // `arc` joins from the current point, so a path left behind by anything
    // else would arrive here as a stray line across the glyph.
    cr.new_path();
    cr.set_line_cap(gtk::cairo::LineCap::Round);
    cr.set_line_join(gtk::cairo::LineJoin::Round);

    match tag {
        // The contactless mark: waves leaving a point off to the left, which is
        // the shape everyone already reads as "hold your phone here".
        Tag::Pending => {
            cr.set_source_rgba(0.475, 0.514, 0.612, 1.0);
            cr.set_line_width((r * 0.24).max(1.2));
            // Struck from a point off to the left, and that point is placed so
            // the arcs -- not their centre -- end up centred in the box.
            let origin = cx - r * 0.92;
            for step in 1..=3 {
                cr.new_path();
                cr.arc(origin, cy, r * (0.4 * step as f64 + 0.18), -0.8, 0.8);
                let _ = cr.stroke();
            }
        }
        // A ring with the waves broken across it: the same mark as above,
        // struck through, which is what "this is not going to work" looks like
        // without a word of explanation.
        Tag::Unseen => {
            cr.set_source_rgba(0.788, 0.639, 0.373, 1.0);
            cr.set_line_width((r * 0.20).max(1.2));
            let origin = cx - r * 0.92;
            for step in 1..=3 {
                cr.new_path();
                cr.arc(origin, cy, r * (0.4 * step as f64 + 0.18), -0.8, 0.8);
                let _ = cr.stroke();
            }
            cr.new_path();
            cr.set_line_width((r * 0.22).max(1.2));
            cr.move_to(cx - r * 0.72, cy + r * 0.72);
            cr.line_to(cx + r * 0.72, cy - r * 0.72);
            let _ = cr.stroke();
        }
        // A ring and a tick. Green, and the only green on the page, so it
        // carries across a room without anything else having to change.
        Tag::Scanned => {
            cr.set_source_rgba(0.451, 0.820, 0.620, 1.0);
            cr.set_line_width((r * 0.20).max(1.2));
            cr.arc(cx, cy, r * 0.9, 0.0, TAU);
            let _ = cr.stroke();

            cr.set_line_width((r * 0.26).max(1.4));
            cr.move_to(cx - r * 0.40, cy + r * 0.02);
            cr.line_to(cx - r * 0.11, cy + r * 0.32);
            cr.line_to(cx + r * 0.44, cy - r * 0.30);
            let _ = cr.stroke();
        }
    }
}

/// Repaint a page as one whose time is served and which is now waiting on the
/// tag. The countdown is not left sitting at 0:00: a clock that has stopped
/// reads as a bug, and the one thing this page has to do is say what it wants.
fn ask_for_the_tag(page: &Page, ask: &str) {
    page.title.set_text("Break's over");
    page.count.remove_css_class("tea-count");
    page.count.add_css_class("tea-await");
    // A clock is four characters and never wraps; this is a sentence somebody
    // wrote, and it lands in the middle of the screen where the ring used to
    // be. Left to itself it would run off both edges.
    page.count.set_wrap(true);
    page.count.set_justify(gtk::Justification::Center);
    page.count.set_max_width_chars(WRAP_AT);
    // Two lines, and then an ellipsis. The column's height is what keeps the
    // words clear of the screen edges on a small panel -- see the layout test
    // -- and a prompt somebody wrote a paragraph into must not be what breaks
    // it. Two lines at this size is about fifty characters, which is a
    // sentence.
    page.count.set_lines(2);
    page.count.set_ellipsize(gtk::pango::EllipsizeMode::End);
    page.count.set_text(ask);
    page.sub.set_text("The page lifts the moment it hears from you.");
    // The time really is spent, so the ring stops being drawn -- see `Dial`.
    let mut dial = page.dial.borrow_mut();
    dial.remaining = 0.0;
    dial.spent = true;
}

/// The walk paid off. On a page that was waiting this is what replaces the
/// asking -- for the second or so before the page comes down, which is exactly
/// long enough to see that it worked.
fn thank_them(page: &Page) {
    page.count.set_text("Off you go");
    page.sub.set_text("That's the break done.");
}

/// Roughly where the prompt wraps, in characters. Wide enough for a sentence,
/// narrow enough that it never reaches the edges of a laptop screen.
const WRAP_AT: i32 = 24;

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
    face: &Face,
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

    // The subtitle and the badge share the lower slot the way the mark and the
    // title share the upper one: grouped and centred inside it, so the
    // countdown stays on the exact centre of the screen whether or not there is
    // a tag to report on.
    let badge = face.tag.map(Badge::new);
    let feet = gtk::Box::new(gtk::Orientation::Vertical, 14);
    feet.set_halign(gtk::Align::Center);
    feet.set_valign(gtk::Align::Center);
    feet.set_vexpand(true);
    feet.append(&sub);
    if let Some(badge) = &badge {
        feet.append(&badge.row);
    }

    let foot = gtk::Box::new(gtk::Orientation::Vertical, 0);
    foot.set_size_request(-1, slot);
    foot.append(&feet);

    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.append(&head);
    column.append(&count);
    column.append(&foot);
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
    animate_in(&foot, arrival * 0.34, 0, arrival * 0.58);

    // Insurance: if the frame clock never delivers -- a stalled
    // compositor, a machine thrashing on resume -- an overlay stuck
    // part-way through would be an invisible break. Force the finished
    // state once the animation has had more than long enough.
    let settled: Vec<gtk::Widget> =
        vec![count.clone().upcast(), head.clone().upcast(), foot.clone().upcast()];
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

    let page = Page {
        win,
        monitor: monitor.clone(),
        count,
        title,
        sub,
        badge,
        dial: state,
    };
    if let Some(ask) = &face.ask {
        ask_for_the_tag(&page, ask);
    }
    if face.done {
        thank_them(&page);
    }
    page
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
// One argument more than clippy's taste allows, and each is a distinct thing
// the loop needs: what to rebuild, how, how often, whether to still be doing it
// at all, who to ask about input, and what a new page should say.
#[allow(clippy::too_many_arguments)]
fn insist(
    app: &gtk::Application,
    pages: Rc<RefCell<Vec<Page>>>,
    total: Duration,
    anim: Anim,
    every: Duration,
    live: Rc<Cell<bool>>,
    session: Rc<RefCell<Session>>,
    facing: impl Fn() -> Face + 'static,
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
        let face = facing();
        let fresh: Vec<Page> = showing
            .iter()
            .map(|page| {
                let left = Duration::from_secs_f64(page.dial.borrow().remaining.max(0.0));
                build_page(&app, &page.monitor, total, left, &anim, Entrance::None, &face)
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
    /// The countdown is over and the page is only waiting. The ring stops being
    /// drawn at all: an empty circle with a sentence running through it is
    /// worse than no circle, and the middle of the screen is needed for the
    /// sentence.
    spent: bool,
    /// 0 to 1 while it arrives, then stays at 1.
    entrance: f64,
    /// `Some` from the moment the scan lands, running 0 to 1 over
    /// [`CELEBRATE`] seconds while the stage plays the confetti. `None` on a
    /// page that has never seen a scan -- including one rebuilt by `insist`
    /// mid-play: a celebration replayed on every rebuild stops being one.
    celebrate: Option<f64>,
    last_frame: i64,
}

/// How long the confetti plays after a scan — and, when the scan is what ends
/// the break, how long the page stays up to play it: see `release`. Three
/// seconds: long enough to land as a reward, short enough that the desk it
/// just unlocked is not held hostage by its own applause.
const CELEBRATE: f64 = 3.0;

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
        spent: false,
        // With the animation switched off, start already arrived.
        entrance: if arrival <= 0.0 { 1.0 } else { 0.0 },
        celebrate: None,
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

        // The scan landed: the celebration, over whatever the page is showing
        // -- the waiting words, or a countdown that still has to run.
        if let Some(t) = dial.celebrate {
            if t < 1.0 {
                draw_celebration(cr, cx, cy, reach, t);
            }
        }

        // Nothing left to count. The backdrop is the whole page from here, and
        // the words that replaced the clock get the room the ring was using.
        if dial.spent {
            return;
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
            if let Some(t) = dial.celebrate {
                dial.celebrate = Some((t + delta / CELEBRATE).min(1.0));
            }
        }
        area.queue_draw();
        glib::ControlFlow::Continue
    });

    (area, state)
}

/// Confetti colours, every one already on the page: the badge's green, the
/// dial's two blues, and the unseen badge's amber. A celebration in colours
/// the page has never used would look pasted on.
const CONFETTI: [(f64, f64, f64); 4] = [
    (0.451, 0.820, 0.620),
    (0.48, 0.63, 0.97),
    (0.62, 0.74, 1.0),
    (0.788, 0.639, 0.373),
];

/// The walk paid off: one green wave and a sky of confetti, launched from the
/// middle of the page and sinking as it fades. Varied without randomness, the
/// same way the shards are -- every scan earns the same celebration, and
/// nothing here needs a seed.
fn draw_celebration(cr: &gtk::cairo::Context, cx: f64, cy: f64, reach: f64, t: f64) {
    let fade = (1.0 - t).powi(2);

    // The wave first: the badge's green, and the only green ring this page
    // ever draws, so "that worked" reads from across the room.
    let wave = ease_out_cubic(t);
    cr.new_path();
    cr.set_line_width((7.0 * (1.0 - t)).max(0.5));
    cr.set_source_rgba(0.451, 0.820, 0.620, fade * 0.85);
    cr.arc(cx, cy, (reach * 0.6 * wave).max(1.0), 0.0, TAU);
    let _ = cr.stroke();

    const PIECES: u32 = 60;
    let flown = ease_out_cubic(t);
    for i in 0..PIECES {
        let angle = i as f64 / PIECES as f64 * TAU + 0.7;
        let speed = 0.30 + ((i * 11) % 7) as f64 * 0.11;
        let distance = reach * 0.5 * flown * speed;
        // Heavier pieces sink sooner. The sag is what makes this confetti
        // rather than shrapnel: the blast flies straight, a celebration falls.
        let sink = reach * 0.10 * t * t * (1.0 + ((i * 5) % 3) as f64);
        let x = cx + angle.cos() * distance;
        let y = cy + angle.sin() * distance * 0.85 + sink;

        let (r, g, b) = CONFETTI[(i % 4) as usize];
        cr.set_source_rgba(r, g, b, fade);

        // Little rectangles, each tumbling at its own rate.
        let spin = angle + t * (2.0 + ((i * 3) % 5) as f64);
        let size = 3.0 + ((i * 13) % 4) as f64 * 1.5;
        let _ = cr.save();
        cr.translate(x, y);
        cr.rotate(spin);
        cr.rectangle(-size / 2.0, -size / 4.0, size, size / 2.0);
        let _ = cr.fill();
        let _ = cr.restore();
    }
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
    fn a_page_rebuilt_mid_break_remembers_the_walk() {
        const PROMPT: &str = "Scan the tag in the hall";
        let prompt = Some(PROMPT);

        // Nothing gating the break: no badge at all, on a page that has never
        // heard of a tag.
        assert!(face_of(None, false, false, true).tag.is_none());
        assert!(face_of(None, true, true, true).tag.is_none());

        // Gated: the badge tracks the scan, and survives whatever rebuilds the
        // page -- insisting, or a monitor arriving mid-break.
        assert_eq!(face_of(prompt, false, false, true).tag, Some(Tag::Pending));
        assert_eq!(face_of(prompt, false, true, true).tag, Some(Tag::Scanned));

        // Nothing watching: the page says so rather than asking for a walk that
        // would not be noticed. A scan already made outranks it.
        assert_eq!(face_of(prompt, false, false, false).tag, Some(Tag::Unseen));
        assert_eq!(face_of(prompt, false, true, false).tag, Some(Tag::Scanned));

        // Waiting *and* scanned is a page about to come down, and it says so
        // rather than still asking -- however many times it gets rebuilt in the
        // second before it goes.
        assert!(!face_of(prompt, false, true, true).done, "not waiting yet");
        assert!(!face_of(prompt, true, false, true).done, "waiting, nobody has been");
        assert!(face_of(prompt, true, true, true).done);

        // The full waiting page only once the countdown is actually spent.
        assert_eq!(face_of(prompt, false, false, true).ask, None);
        assert_eq!(face_of(prompt, true, false, true).ask, Some(PROMPT.to_string()));
    }

    #[test]
    fn the_two_tag_states_never_read_the_same() {
        assert_ne!(words_for(Tag::Pending), words_for(Tag::Scanned));
    }

    #[test]
    fn soft_is_the_default_and_does_not_insist() {
        assert!(!Hold::default().insists());
        assert!(Hold { mode: Grip::Insist, ..Hold::default() }.insists());
    }

    /// The layout has one job: the words go outside the ring, on every screen.
    #[test]
    fn the_text_always_clears_the_dial() {
        // The subtitle, the gap, and the tag badge under it -- the tallest the
        // lower slot ever gets, since the badge is the only thing that can be
        // added to it. Generous on purpose: it is the number that decides
        // whether the words land on the ring.
        const SUB_HEIGHT: f64 = 64.0;

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

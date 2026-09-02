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
use crate::nfc::{Motion, Walk};
use crate::session::Session;
use tea_core::{Blocker, Snooze};
use serde::Deserialize;
use std::cell::{Cell, RefCell};
use std::f64::consts::{FRAC_PI_2, TAU};
use std::rc::Rc;
use std::time::Duration;

/// One typeface for the whole thing, and a monospaced one: this page is a
/// clock, a count and two labels that change under you, and proportional type
/// makes all three twitch as their digits change width. The stack is only
/// names -- whichever of them the machine actually has is the one it uses.
const MONO: &str = "\"JetBrains Mono\", \"CaskaydiaMono NF\", \"Cascadia Mono\", \
                    \"IBM Plex Mono\", \"Ubuntu Sans Mono\", \"Noto Sans Mono\", \
                    \"DejaVu Sans Mono\", monospace";

const CSS: &str = "
window.tea-overlay { background-color: transparent; font-family: MONO; }
.tea-backdrop { background-color: #06080d; }
window.tea-toast {
    background-color: #101522;
    border-radius: 16px;
    font-family: MONO;
}
window.tea-toast button {
    background-image: none;
    background-color: rgba(ACCENT,0.10);
    color: #dce4f5;
    border: 1px solid rgba(ACCENT,0.22);
    border-radius: 999px;
    padding: 8px 18px;
    font-size: 11pt;
    letter-spacing: 1px;
}
window.tea-toast button:hover { background-color: rgba(ACCENT,0.18); }

/* A pill: the shape everything on this page that is not the clock arrives in. */
.tea-chip {
    background-color: rgba(ACCENT,0.05);
    border: 1px solid rgba(ACCENT,0.15);
    border-radius: 999px;
    padding: 7px 18px;
}
.tea-chip-word { font-size: 10pt; letter-spacing: 4px; color: #93a3c0; }

.tea-title { font-size: 34pt; font-weight: 300; color: #eef2fa; letter-spacing: 2px; }
.tea-count { font-size: 58pt; font-weight: 300; color: #ffffff; letter-spacing: 4px; }
.tea-caption { font-size: 8pt; letter-spacing: 6px; color: #5d6880; }
.tea-await { font-size: 26pt; font-weight: 300; color: #eef2fa; letter-spacing: 1px; }
.tea-sub   { font-size: 12pt; color: #78849c; letter-spacing: 1px; }
.tea-tag       { font-size: 11pt; color: #9aa9c4; letter-spacing: 1px; }
.tea-tag.done  { color: #79dca8; }
.tea-tag.unseen { color: #d8b06a; }
.tea-warn-text { font-size: 15pt; color: #e6e9f0; }
.tea-warn-sub  { font-size: 11pt; color: #78849c; letter-spacing: 1px; }
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
    /// The colours, resolved once from `[page]`.
    palette: Palette,
    /// What the page says under the title, and when it changes its mind.
    /// Shared with the face closure, so a page rebuilt by `insist` comes back
    /// saying the same line as the one it replaced.
    prompter: Rc<RefCell<Prompter>>,
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
    /// How far the walk has got, when this break is also being walked off.
    /// Shared for the same reason again -- a page rebuilt at step eighteen has
    /// to come back saying eighteen.
    walk: Rc<Cell<Walk>>,
    /// And the time on your feet, shared for the same reason.
    motion: Rc<Cell<Motion>>,
    /// The steps came from a hand: see `cheat_seen`. Shared like the rest.
    busted: Rc<Cell<bool>>,
    /// Both halves are in and the break is ending. Distinct from `scanned`,
    /// which is only the tag: the celebration belongs to the moment the page is
    /// actually about to lift, not to the first of two things that had to
    /// happen.
    open: Rc<Cell<bool>>,
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
        look: &Look,
        ask: Option<String>,
    ) -> Self {
        if let Some(display) = gdk::Display::default() {
            let provider = gtk::CssProvider::new();
            provider.load_from_string(&look.css(CSS));
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
            palette: look.palette(),
            prompter: Rc::new(RefCell::new(Prompter::new(look))),
            pages: Rc::new(RefCell::new(Vec::new())),
            warning: None,
            ask,
            waiting: Rc::new(Cell::new(false)),
            scanned: Rc::new(Cell::new(false)),
            reachable: Rc::new(Cell::new(true)),
            walk: Rc::new(Cell::new(Walk::default())),
            motion: Rc::new(Cell::new(Motion::default())),
            busted: Rc::new(Cell::new(false)),
            open: Rc::new(Cell::new(false)),
            live: Rc::new(Cell::new(false)),
            session: Rc::new(RefCell::new(Session::connect())),
            monitors_watch: None,
        }
    }
}

/// Everything a page shows that is not the clock.
#[derive(Clone, Default)]
struct Face {
    /// Waiting, and the gate has opened: the page is a second from coming down
    /// and should say so rather than still asking. Rebuilt pages included --
    /// `insist` can replace one between the scan and the page lifting.
    done: bool,
    /// Set once the countdown is spent and the page is only waiting: what to
    /// ask for, in the user's own words.
    ask: Option<String>,
    /// `None` when nothing is gating this break, and the page carries no badge
    /// at all -- a break that ends by itself has nothing to report.
    tag: Option<Tag>,
    /// `None` when this break has no walk in it.
    walk: Option<Walk>,
    /// `None` when this break does not ask for time on your feet.
    motion: Option<Motion>,
    /// The steps badge is teasing rather than counting.
    busted: bool,
    /// The line under the title while the clock runs, when `[page].prompts`
    /// has given the page something to say. `None` keeps the line it ships with.
    sub: Option<String>,
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
        let walk = Rc::clone(&self.walk);
        let motion = Rc::clone(&self.motion);
        let busted = Rc::clone(&self.busted);
        let open = Rc::clone(&self.open);
        let ask = self.ask.clone();
        let prompter = Rc::clone(&self.prompter);
        move || {
            let mut face = face_of(
                ask.as_deref(),
                waiting.get(),
                scanned.get(),
                reachable.get(),
                open.get(),
                walk.get(),
                motion.get(),
                busted.get(),
            );
            face.sub = prompter.borrow().current();
            face
        }
    }

    /// What the current face is, without building a page from it.
    fn facing(&self) -> Face {
        (self.face())()
    }

    /// The waiting page's big line, worked out again.
    ///
    /// What the page is asking for changes as the gate closes: with the tag in
    /// and a walk still to do, a page still saying "scan the tag" is sending
    /// somebody back down the hall for something they have already done.
    fn retext(&self) {
        if !self.waiting.get() {
            return;
        }
        let face = self.facing();
        for page in self.pages.borrow().iter() {
            if face.done {
                thank_them(page);
            } else if let Some(ask) = &face.ask {
                ask_for_the_tag(page, ask);
            }
        }
    }
}

/// Split out from the closure above so the rules can be checked without a
/// display: what a page shows is decided here, and a rebuilt page that forgot
/// a scan would send someone back down the hall for nothing.
// Every input the page's face depends on, and nothing else: a struct here
// would be this list with a name.
#[allow(clippy::too_many_arguments)]
fn face_of(
    ask: Option<&str>,
    waiting: bool,
    scanned: bool,
    reachable: bool,
    open: bool,
    walk: Walk,
    motion: Motion,
    busted: bool,
) -> Face {
    let walk = (walk.needed > 0).then_some(walk);
    let motion = (motion.needed > 0).then_some(motion);
    Face {
        done: waiting && open,
        // Only once the countdown is spent. Before that the page has a clock to
        // show, and the badges below say all that needs saying about the gate.
        ask: match (waiting, scanned, walk) {
            (false, ..) => None,
            // The tag is in and the walk is not: asking for the tag again would
            // send someone back down the hall for a thing they have done. What
            // is left is the only thing worth saying.
            // Caught out: the page says so instead of asking for more steps,
            // until the phone reports moving and the badge goes back to counting.
            (true, _, Some(w)) if busted && !w.done() => Some(CAUGHT.to_string()),
            (true, true, Some(w)) if !w.done() => Some(match w.left() {
                1 => "One more step".to_string(),
                left => format!("{left} more steps"),
            }),
            // Tag in, steps in, and only the phone's word still wanted: say
            // what to do, not what to scan.
            (true, true, _) if motion.is_some_and(|m| !m.done() && !m.lost) => {
                Some(format!("Keep walking — {}s more", motion.map_or(0, |m| m.left())))
            }
            (true, ..) => ask.map(str::to_string),
        },
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
        walk: ask.and(walk),
        motion: ask.and(motion),
        busted: busted && ask.and(walk).is_some(),
        sub: None,
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
        // The gate too, and not only its tag half: left standing from the last
        // break, `done` would be true the moment this one starts waiting, and
        // the page would say "off you go" while still holding the screen.
        self.open.set(false);
        self.reachable.set(true);
        // A fresh break starts the prompts somewhere new, so five breaks in a
        // row do not all open with the same line.
        self.prompter.borrow_mut().begin();

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
                build_page(
                    &self.app,
                    monitor,
                    total,
                    total,
                    &self.anim,
                    &self.palette,
                    Entrance::Full,
                    &face,
                )
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
                self.palette,
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
        let palette = self.palette;
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
                .map(|m| build_page(&app, m, total, left, &anim, &palette, Entrance::None, &face))
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
        // Once a second, which is what the prompts count in. Only while the
        // clock runs: the waiting page has its own line, and a stretch
        // suggested to somebody standing in the hall is noise.
        // The prompts keep their beat under a verdict, but do not overwrite it.
        let line = self.prompter.borrow_mut().tick();
        if let Some(line) = line
            && !self.busted.get()
        {
            for page in self.pages.borrow().iter() {
                page.sub.set_text(&line);
            }
        }
        for page in self.pages.borrow().iter() {
            // The dial runs itself between ticks so the sweep is smooth, and
            // so that the clock moves on when the second does rather than when
            // the host gets round to saying so. This is the once-a-second
            // correction back to what the scheduler says, which is what keeps
            // the free-running countdown from drifting away from it.
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
                badge.paint(Mark::Tag(tag));
                animate_in(&badge.row, 0.25, 0, 0.0);
            }
        }
    }

    fn tag_seen(&mut self) {
        // Called on every tick the scan is still banked, so that a page resumed
        // after a restart comes back green. Doing the work once is the point:
        // repainting a label every second is how a page ends up flickering at
        // someone who is trying to rest.
        if self.scanned.replace(true) {
            return;
        }
        for page in self.pages.borrow().iter() {
            if let Some(badge) = &page.badge {
                badge.paint(Mark::Tag(Tag::Scanned));
                // A quarter of a second of fade, so the change registers as
                // something that just happened rather than something that was
                // always there.
                animate_in(&badge.row, 0.25, 0, 0.0);
            }
        }
        // With a walk still to do, the big text has been asking for a tag that
        // is now in. It has to stop, and say what is actually left.
        self.retext();
    }

    fn steps_seen(&mut self, walked: u32, needed: u32, marked: bool) {
        let walk = Walk { walked, needed, marked };
        if self.walk.replace(walk) == walk {
            return;
        }
        for page in self.pages.borrow().iter() {
            if let Some(badge) = &page.walk {
                badge.paint(match self.busted.get() {
                    true => Mark::Cheat(walk),
                    false => Mark::Walk(walk),
                });
                // Only the finish is worth a fade. A badge that flashes on
                // every step counted is a strobe at the foot of a page whose
                // one job is to be restful.
                if walk.done() {
                    animate_in(&badge.row, 0.25, 0, 0.0);
                }
            }
            if let Some(meter) = &page.meter {
                meter.paint(walk);
            }
        }
        self.retext();
    }

    fn motion_seen(&mut self, secs: u32, needed: u32, lost: bool) {
        let motion = Motion { secs, needed, lost };
        if self.motion.replace(motion) == motion {
            return;
        }
        for page in self.pages.borrow().iter() {
            if let Some(badge) = &page.motion {
                badge.paint(Mark::Move(motion));
                if motion.done() {
                    animate_in(&badge.row, 0.25, 0, 0.0);
                }
            }
        }
        self.retext();
    }

    fn cheat_seen(&mut self, busted: bool) {
        if self.busted.replace(busted) == busted {
            return;
        }
        let walk = self.walk.get();
        let waiting = self.waiting.get();
        for page in self.pages.borrow().iter() {
            if let Some(badge) = &page.walk {
                badge.paint(match busted {
                    true => Mark::Cheat(walk),
                    false => Mark::Walk(walk),
                });
                if busted {
                    animate_in(&badge.row, 0.25, 0, 0.0);
                }
            }
            // While the clock runs the verdict goes under the title, where the
            // prompts go. The waiting page says it in the big line instead,
            // which `retext` sees to.
            if !waiting {
                let line = match busted {
                    true => CAUGHT.to_string(),
                    false => self.prompter.borrow().current().unwrap_or_else(|| SUB.to_string()),
                };
                page.sub.set_text(&line);
            }
        }
        self.retext();
    }

    fn release_seen(&mut self) {
        // Both halves are in. Told every tick from here on, so like the two
        // above it does its work exactly once.
        self.tag_seen();
        if self.open.replace(true) {
            return;
        }
        // The walk badge is deliberately not touched here: a break freed by its
        // grace, or by a hub that went away, must not end with the page
        // claiming a walk that nobody made.
        let waiting = self.waiting.get();
        for page in self.pages.borrow().iter() {
            if waiting {
                thank_them(page);
            }
            // The walk happened: confetti, on every screen at once. A page
            // that was only waiting for this stays up long enough to play it
            // -- see `release`.
            page.dial.borrow_mut().celebrate = Some(0.0);
        }
    }

    fn await_release(&mut self) {
        if self.ask.is_none() {
            // Nothing is gating this break, so there is nothing to ask for and
            // the page is about to come down anyway.
            return;
        }
        // Set first: what the page asks for depends on it, and on what of the
        // gate is already in -- a tag scanned during the countdown leaves only
        // the steps to ask about.
        self.waiting.set(true);
        if let Some(ask) = self.facing().ask {
            println!("[wait]  time served — {ask}");
        }
        self.retext();
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

/// The line under the title, until the prompts have something else to say.
const SUB: &str = "Look away from the screen. Stand up.";
/// What the page says when the steps came from a hand.
const CAUGHT: &str = "That was the phone walking, not you.";
/// Where a prompt wraps. Wide enough for a sentence, narrow enough that it
/// stays a caption under the title rather than a paragraph across the screen.
const SUB_WIDTH: i32 = 56;
/// The accent the page ships with, as the CSS has always had it.
const ACCENT: &str = "#7aa2ff";

/// What the page says while the clock runs, when it says anything. Short, and
/// all things that can be done beside a desk: the walk is what the tag is
/// for, this is what to do with the minutes at either end of it.
const PROMPTS: &[&str] = &[
    "Look at something far away. Twenty seconds.",
    "Roll your shoulders back. Slowly, five times.",
    "Drink some water.",
    "Stand up and reach for the ceiling.",
    "Breathe in for four, out for six. Three times.",
    "Turn your head to one side, then the other. Hold each.",
    "Unclench your jaw. Drop your shoulders.",
    "Walk to a window.",
    "Blink. Properly, a few times.",
    "Palm out, fingers back: stretch each wrist.",
    "Stand on one leg. Then the other.",
    "Straighten your back. Ears over shoulders.",
];

/// How the page looks, and what it says while the clock runs: `[page]`.
///
/// The defaults are the page as it ships, pixel for pixel. Everything here is
/// a departure from that, made on purpose.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Look {
    /// `#rrggbb`: the ring, the glow, the blast and the pills. Not the text.
    pub accent: String,
    pub background: Background,
    /// A family name. Empty takes the first monospaced face the machine has.
    pub font: String,
    pub prompts: Prompts,
    /// How long each prompt stays up.
    pub prompt_every: Dur,
}

impl Default for Look {
    fn default() -> Self {
        Self {
            accent: ACCENT.into(),
            background: Background::Dark,
            font: String::new(),
            prompts: Prompts::Off,
            prompt_every: Dur(Duration::from_secs(20)),
        }
    }
}

/// What is behind the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Background {
    /// The screen is covered.
    #[default]
    Dark,
    /// The desk shows through, darkened: a veil over the work rather than a
    /// wall in front of it.
    Dim,
}

/// `"off"`, `"on"` for the built-in list, or a list of lines of your own.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(try_from = "RawPrompts")]
pub enum Prompts {
    #[default]
    Off,
    On,
    Custom(Vec<String>),
}

/// The two shapes the key can take in the file.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawPrompts {
    Word(String),
    List(Vec<String>),
}

impl TryFrom<RawPrompts> for Prompts {
    type Error = String;

    fn try_from(raw: RawPrompts) -> Result<Self, String> {
        match raw {
            RawPrompts::Word(word) => match word.trim().to_ascii_lowercase().as_str() {
                "off" | "disabled" | "false" => Ok(Prompts::Off),
                "on" | "enabled" | "true" => Ok(Prompts::On),
                other => Err(format!(
                    "page.prompts: {other:?} is not \"off\", \"on\", or a list of lines"
                )),
            },
            RawPrompts::List(lines) => {
                let lines: Vec<String> = lines
                    .into_iter()
                    .map(|line| line.trim().to_string())
                    .filter(|line| !line.is_empty())
                    .collect();
                // An empty list is "off", written the long way.
                Ok(if lines.is_empty() { Prompts::Off } else { Prompts::Custom(lines) })
            }
        }
    }
}

impl Prompts {
    pub fn on(&self) -> bool {
        !matches!(self, Prompts::Off)
    }

    /// The lines the page will cycle through. Empty when off.
    pub fn lines(&self) -> Vec<String> {
        match self {
            Prompts::Off => Vec::new(),
            Prompts::On => PROMPTS.iter().map(|s| s.to_string()).collect(),
            Prompts::Custom(lines) => lines.clone(),
        }
    }

    /// How it reads in `tea config`.
    pub fn describe(&self) -> String {
        match self {
            Prompts::Off => "off".into(),
            Prompts::On => format!("on — {} built-in lines", PROMPTS.len()),
            Prompts::Custom(lines) => format!("{} of your own", lines.len()),
        }
    }
}

impl Look {
    /// The accent as it was written, as bytes, if it was written properly.
    fn rgb(&self) -> Option<(u8, u8, u8)> {
        let hex = self.accent.trim().strip_prefix('#')?;
        if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        Some((byte(0)?, byte(2)?, byte(4)?))
    }

    /// Why the accent is being ignored, if it is. Said once at startup, and
    /// then the page uses the colour it ships with: a typo in a colour should
    /// cost a colour, not a break.
    pub fn accent_misconfigured(&self) -> Option<String> {
        match self.rgb() {
            Some(_) => None,
            None => Some(format!(
                "page.accent {:?} is not a colour like \"#7aa2ff\" — using the default",
                self.accent
            )),
        }
    }

    /// The stylesheet with the accent and the face filled in.
    fn css(&self, css: &str) -> String {
        let (r, g, b) = self.rgb().unwrap_or((0x7a, 0xa2, 0xff));
        let family = match self.font.trim() {
            "" => MONO.to_string(),
            // Quoted, and never able to end the quote early: a family name is
            // one string in the stylesheet, whatever was typed.
            face => format!("\"{}\", {MONO}", face.replace('"', "")),
        };
        css.replace("ACCENT", &format!("{r},{g},{b}")).replace("MONO", &family)
    }

    /// The colours the page is painted in, worked out once.
    pub(crate) fn palette(&self) -> Palette {
        let stock = self.accent.trim().eq_ignore_ascii_case(ACCENT);
        let mut palette = match self.rgb() {
            // The colours as they ship are not derived from the accent -- they
            // were chosen one by one -- so the stock accent gets them exactly.
            Some(rgb) if !stock => Palette::from_accent(rgb),
            _ => Palette::default(),
        };
        palette.cover = match self.background {
            Background::Dark => 1.0,
            Background::Dim => DIM,
        };
        palette
    }
}

/// How much of the desk the dark covers with `background = "dim"`. Enough that
/// nothing behind it can be read, which is the point of the page; little
/// enough that the shapes are there, which is the point of the setting.
const DIM: f64 = 0.84;

/// Every colour the page draws with that is not fixed. Cairo wants floats, so
/// these are floats; the stylesheet gets the same accent in bytes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Palette {
    /// The ring's arc, and the lit squares of the meter.
    pub accent: (f64, f64, f64),
    /// The blast's waves and the graph paper.
    pub wave: (f64, f64, f64),
    /// The shards, and the paler confetti.
    pub light: (f64, f64, f64),
    /// The darker confetti.
    pub deep: (f64, f64, f64),
    /// The circle the arc runs on.
    pub hairline: (f64, f64, f64),
    /// The breath of light behind the dial.
    pub glow: (f64, f64, f64),
    pub pip: (f64, f64, f64),
    pub pip_unlit: (f64, f64, f64),
    /// How opaque the dark is. One covers the screen.
    pub cover: f64,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            accent: (0.56, 0.71, 1.0),
            wave: (0.55, 0.70, 1.0),
            light: (0.62, 0.74, 1.0),
            deep: (0.48, 0.63, 0.97),
            hairline: (0.20, 0.28, 0.47),
            glow: (0.29, 0.42, 0.85),
            pip: (0.54, 0.69, 1.0),
            pip_unlit: (0.42, 0.52, 0.72),
            cover: 1.0,
        }
    }
}

impl Palette {
    /// The whole set from one colour, keeping the same relationships the
    /// stock colours have to their accent: the hairline is the accent most of
    /// the way to the dark, the shards a shade lighter than the arc.
    fn from_accent((r, g, b): (u8, u8, u8)) -> Self {
        let accent = (f64::from(r) / 255.0, f64::from(g) / 255.0, f64::from(b) / 255.0);
        let dark = (0.024, 0.031, 0.051);
        let white = (1.0, 1.0, 1.0);
        let towards = |(r, g, b): (f64, f64, f64), (tr, tg, tb): (f64, f64, f64), k: f64| {
            (r + (tr - r) * k, g + (tg - g) * k, b + (tb - b) * k)
        };
        Self {
            accent,
            wave: accent,
            light: towards(accent, white, 0.18),
            deep: towards(accent, dark, 0.12),
            hairline: towards(accent, dark, 0.62),
            glow: towards(accent, dark, 0.35),
            pip: accent,
            pip_unlit: towards(accent, dark, 0.27),
            cover: 1.0,
        }
    }

    /// Confetti colours, every one already on the page: the badge's green, the
    /// dial's two blues, and the unseen badge's amber. A celebration in colours
    /// the page has never used would look pasted on.
    fn confetti(&self) -> [(f64, f64, f64); 4] {
        [GREEN, self.deep, self.light, AMBER]
    }
}

/// The prompts, and which one is up.
///
/// Counts in ticks of the engine's clock rather than in wall time, so the
/// line changes on the same beat the numbers do, and a page rebuilt by
/// `insist` mid-line comes back saying the line and not the next one.
struct Prompter {
    lines: Vec<String>,
    /// Ticks each line stays up.
    every: u64,
    at: usize,
    ticks: u64,
}

impl Prompter {
    fn new(look: &Look) -> Self {
        Self {
            lines: look.prompts.lines(),
            // Five seconds is already too quick to read and act on; ten minutes
            // is longer than a break. Outside that a typo, not a wish.
            every: look.prompt_every.0.as_secs().clamp(5, 600),
            at: 0,
            ticks: 0,
        }
    }

    /// A break is starting: begin somewhere new.
    fn begin(&mut self) {
        let start = match self.lines.len() {
            0 | 1 => 0,
            n => glib::random_int_range(0, n as i32) as usize,
        };
        self.begin_at(start);
    }

    fn begin_at(&mut self, at: usize) {
        self.ticks = 0;
        self.at = if self.lines.is_empty() { 0 } else { at % self.lines.len() };
    }

    /// What the page should be saying now, or nothing when prompts are off.
    fn current(&self) -> Option<String> {
        self.lines.get(self.at).cloned()
    }

    /// One second on. `Some` when the line has just changed, carrying the new
    /// one; a page only has to touch its label then.
    fn tick(&mut self) -> Option<String> {
        if self.lines.len() < 2 {
            return None;
        }
        self.ticks += 1;
        if !self.ticks.is_multiple_of(self.every) {
            return None;
        }
        self.at = (self.at + 1) % self.lines.len();
        self.current()
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
    /// The "REMAINING" under the clock, and the blank above it that keeps the
    /// clock centred. Both go when the countdown does: a page waiting on a tag
    /// has a sentence where the numbers were, and nothing left to caption.
    caption: gtk::Box,
    pad: gtk::Box,
    /// Absent unless a tag is what ends this break.
    badge: Option<Badge>,
    /// Absent unless the break is being walked off as well.
    walk: Option<Badge>,
    /// Absent unless the break also wants the phone's word that you moved.
    motion: Option<Badge>,
    /// The block of squares under the badges, same condition as `walk`.
    meter: Option<Meter>,
    dial: Rc<RefCell<Dial>>,
}

/// The three layers the page is painted on, and why there are three.
///
/// They used to be one full-screen drawing area redrawn on every frame of the
/// break: the dark, the graph paper, a full-screen gradient and the ring, sixty
/// times a second, on every monitor, for the length of a break. That is a space
/// heater with a clock on it. Split by how often each part actually changes,
/// and the steady state costs a small square in the middle of the screen.
struct Stage {
    /// Never changes once drawn: the dark, the grid, the glow.
    backdrop: gtk::DrawingArea,
    /// The blast and the confetti. Full screen, but hidden except while one of
    /// them is actually playing, which is a few seconds of a five-minute break.
    burst: gtk::DrawingArea,
    /// The ring, in a box just big enough to hold it.
    ring: gtk::DrawingArea,
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
    state: Rc<Cell<Mark>>,
}

/// What one badge is reporting on. Two of them can be on a page at once -- the
/// tag and the walk -- and they are the same thing twice over: a footnote
/// saying whether that half of the gate is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mark {
    Tag(Tag),
    Walk(Walk),
    Move(Motion),
    /// The walk badge, caught out: the count came from a hand.
    Cheat(Walk),
}

impl Mark {
    /// Whether this half is in. Green is spent on exactly this and nothing
    /// else, on a page that is otherwise grey.
    fn done(self) -> bool {
        match self {
            Mark::Tag(tag) => tag == Tag::Scanned,
            Mark::Walk(walk) => walk.done(),
            Mark::Move(motion) => motion.done(),
            Mark::Cheat(_) => false,
        }
    }

    /// Whether the thing this half depends on cannot be read at all -- the
    /// amber state, on a page that is otherwise grey and green.
    fn unseen(self) -> bool {
        match self {
            Mark::Tag(tag) => tag == Tag::Unseen,
            Mark::Walk(_) => false,
            Mark::Move(motion) => motion.lost && !motion.done(),
            Mark::Cheat(_) => true,
        }
    }
}

impl Badge {
    fn new(mark: Mark) -> Self {
        let state = Rc::new(Cell::new(mark));

        let icon = gtk::DrawingArea::new();
        icon.set_content_width(BADGE);
        icon.set_content_height(BADGE);
        icon.set_valign(gtk::Align::Center);
        let drawing = Rc::clone(&state);
        icon.set_draw_func(move |_, cr, width, height| {
            draw_mark(cr, width, height, drawing.get());
        });

        let label = gtk::Label::new(Some(&words_for(mark)));
        label.add_css_class("tea-tag");

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        row.set_halign(gtk::Align::Center);
        row.add_css_class("tea-chip");
        row.append(&icon);
        row.append(&label);

        let badge = Self { row, icon, label, state };
        badge.paint(mark);
        badge
    }

    fn paint(&self, mark: Mark) {
        self.state.set(mark);
        match mark {
            // The count crossed out, and the verdict beside it.
            Mark::Cheat(walk) => {
                self.label.set_markup(&format!("Nice try — <s>{} steps</s>", walk.walked))
            }
            _ => self.label.set_text(&words_for(mark)),
        }
        // One class, toggled, rather than two that could both be on: a label
        // that is somehow "not scanned" and green is worse than no badge.
        for (class, on) in
            [("done", mark.done()), ("unseen", mark.unseen())]
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

/// One square per step, filling as you walk, fifty to a row.
///
/// The badge beside it already says "12 of 20 steps", and this says the same
/// thing again on purpose: a number has to be read, a block of squares that is
/// two thirds full does not. It is the only thing on the page that can be
/// understood from the other side of the room, which is where the person it is
/// for is standing.
#[derive(Clone)]
struct Meter {
    area: gtk::DrawingArea,
    state: Rc<Cell<Walk>>,
}

/// A square, and the air after it.
const PIP: f64 = 9.0;
const PIP_GAP: f64 = 6.0;
/// The air between one row and the next. The same gap as between squares, so
/// the block reads as a grid rather than as rows that happen to be near.
const ROW_GAP: f64 = 6.0;
/// Squares to a row. Past fifty a row stops being a thing you take in at a
/// glance and becomes a ruler you have to read along, so the next fifty go
/// underneath -- and a full row is then worth exactly fifty steps, which is
/// the count itself readable from the doorway.
const PIPS_ROW: u32 = 50;
/// Past this many, one square stops meaning one step and starts meaning a
/// share of the walk -- four rows is a block whose shape can still be seen,
/// twenty is a texture.
const PIPS_MOST: u32 = 4 * PIPS_ROW;

impl Meter {
    fn new(walk: Walk, palette: Palette) -> Self {
        let state = Rc::new(Cell::new(walk));
        let area = gtk::DrawingArea::new();
        let (cols, rows) = Self::grid(walk.needed);
        area.set_content_width((cols as f64 * (PIP + PIP_GAP) - PIP_GAP).ceil() as i32);
        area.set_content_height((rows as f64 * (PIP + ROW_GAP) - ROW_GAP).ceil() as i32);
        area.set_halign(gtk::Align::Center);

        let drawing = Rc::clone(&state);
        area.set_draw_func(move |_, cr, width, height| {
            draw_meter(cr, width, height, drawing.get(), &palette);
        });
        Self { area, state }
    }

    /// How many squares stand for `needed` steps.
    fn pips(needed: u32) -> u32 {
        needed.clamp(1, PIPS_MOST)
    }

    /// The block those squares are laid out in: how wide, and how many rows
    /// deep. Fifty is a full row, so a hundred is two rows and a hundred and
    /// twenty is two rows and twenty. The short row is left-aligned under the
    /// others because that is the order the squares light in.
    fn grid(needed: u32) -> (u32, u32) {
        let pips = Self::pips(needed);
        (pips.min(PIPS_ROW), pips.div_ceil(PIPS_ROW))
    }

    fn paint(&self, walk: Walk) {
        self.state.set(walk);
        self.area.queue_draw();
    }
}

fn draw_meter(cr: &gtk::cairo::Context, width: i32, height: i32, walk: Walk, palette: &Palette) {
    let pips = Meter::pips(walk.needed);
    let (cols, rows) = Meter::grid(walk.needed);
    // Rounded off rather than up: a square that lights before its step has
    // been taken is a promise the gate will not keep.
    let lit = ((walk.walked as f64 / walk.needed.max(1) as f64) * pips as f64).floor() as u32;
    let span = cols as f64 * (PIP + PIP_GAP) - PIP_GAP;
    let tall = rows as f64 * (PIP + ROW_GAP) - ROW_GAP;
    let left = (width as f64 - span) / 2.0;
    let top = (height as f64 - tall) / 2.0;

    for i in 0..pips {
        // Left to right and then down, the way the squares light and the way
        // anybody looking at them reads.
        let x = left + (i % PIPS_ROW) as f64 * (PIP + PIP_GAP);
        let y = top + (i / PIPS_ROW) as f64 * (PIP + ROW_GAP);
        if i < lit {
            let (r, g, b) = palette.pip;
            cr.set_source_rgba(r, g, b, 1.0);
        } else {
            let (r, g, b) = palette.pip_unlit;
            cr.set_source_rgba(r, g, b, 0.22);
        }
        rounded(cr, x, y, PIP, PIP, 2.0);
        let _ = cr.fill();
    }
}

/// A rectangle with its corners taken off. Cairo has no such call, and every
/// square on this page wants one.
fn rounded(cr: &gtk::cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.0).min(h / 2.0);
    cr.new_path();
    cr.arc(x + w - r, y + r, r, -FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, FRAC_PI_2, std::f64::consts::PI);
    cr.arc(x + r, y + r, r, std::f64::consts::PI, 3.0 * FRAC_PI_2);
    cr.close_path();
}

fn words_for(mark: Mark) -> String {
    match mark {
        Mark::Tag(Tag::Pending) => "Tag not scanned yet".to_string(),
        Mark::Tag(Tag::Scanned) => "Tag scanned".to_string(),
        Mark::Tag(Tag::Unseen) => "Can't see the tag — this break ends on the clock".to_string(),
        // The number first, because it is the part that changes and the part
        // being read from across a room.
        Mark::Walk(walk) if walk.done() => format!("{} steps walked", walk.walked),
        // Nothing counted yet, and the reason is not that you have not walked:
        // the first thing your phone said mid-break was a batch from before it,
        // so the count starts again from there. Saying *0 of 50* to somebody
        // who has just crossed the flat sends them across it a second time.
        Mark::Walk(walk) if walk.walked == 0 && walk.marked => "Counting from here".to_string(),
        Mark::Walk(walk) => format!("{} of {} steps", walk.walked, walk.needed),
        Mark::Cheat(walk) => format!("Nice try — {} steps", walk.walked),
        Mark::Move(motion) if motion.done() => "Moved".to_string(),
        Mark::Move(motion) if motion.lost => {
            "Can't read the activity — this break ends on the clock".to_string()
        }
        Mark::Move(motion) if motion.secs == 0 => "Not moving yet".to_string(),
        Mark::Move(motion) => format!("Moving · {}s of {}s", motion.secs, motion.needed),
    }
}

/// How tall the badge's glyph is drawn. Fixed, like the type beside it: this is
/// a footnote, and a footnote that scales with the screen stops being one.
const BADGE: i32 = 22;

/// Two glyphs, drawn rather than pulled from an icon theme -- the same reason
/// the mark on this page is embedded. An icon that is present on the machine
/// you built on and missing on someone else's is a blank square in the middle
/// of the one screen they cannot dismiss.
fn draw_mark(cr: &gtk::cairo::Context, width: i32, height: i32, mark: Mark) {
    match mark {
        Mark::Tag(tag) => draw_tag(cr, width, height, tag),
        // A finished walk gets the same tick a scanned tag does. Two halves of
        // one gate, and when both are in the page says so the same way twice.
        Mark::Walk(walk) if walk.done() => draw_tag(cr, width, height, Tag::Scanned),
        Mark::Walk(walk) => draw_walk(cr, width, height, walk),
        Mark::Cheat(_) => draw_tag(cr, width, height, Tag::Unseen),
        Mark::Move(motion) if motion.done() => draw_tag(cr, width, height, Tag::Scanned),
        // A sensor that cannot be read gets the tag's amber glyph: the same
        // thing has gone wrong, and the page should say so the same way.
        Mark::Move(motion) if motion.lost => draw_tag(cr, width, height, Tag::Unseen),
        // The same figure as the walk: it is the same person, still going.
        Mark::Move(_) => draw_walk(cr, width, height, Walk::default()),
    }
}

/// Somebody walking. Not a progress ring: the squares under the badge
/// is already the progress, and a ring at nought steps is an empty circle that
/// reads as a glyph that failed to load.
fn draw_walk(cr: &gtk::cairo::Context, width: i32, height: i32, _walk: Walk) {
    let unit = width.min(height) as f64 / 22.0;
    cr.new_path();
    cr.set_source_rgba(0.475, 0.514, 0.612, 1.0);
    cr.set_line_cap(gtk::cairo::LineCap::Round);
    cr.set_line_join(gtk::cairo::LineJoin::Round);

    // The head, and then a figure mid-stride: the legs apart is the whole of
    // what makes this read as walking rather than standing.
    cr.arc(11.6 * unit, 4.6 * unit, 2.0 * unit, 0.0, TAU);
    let _ = cr.fill();

    cr.set_line_width(1.9 * unit);
    cr.new_path();
    cr.move_to(11.8 * unit, 8.0 * unit);
    cr.line_to(9.8 * unit, 12.6 * unit);
    let _ = cr.stroke();

    // Front leg, striding out; back leg, trailing.
    cr.new_path();
    cr.move_to(9.8 * unit, 12.6 * unit);
    cr.line_to(13.0 * unit, 17.4 * unit);
    let _ = cr.stroke();
    cr.new_path();
    cr.move_to(9.8 * unit, 12.6 * unit);
    cr.line_to(6.2 * unit, 16.2 * unit);
    let _ = cr.stroke();

    // And an arm, swung the other way, which is what stops it reading as a
    // pair of scissors.
    cr.new_path();
    cr.move_to(11.2 * unit, 9.4 * unit);
    cr.line_to(7.6 * unit, 8.4 * unit);
    let _ = cr.stroke();
}

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
    // Nothing left to caption: the clock is gone and a sentence has the middle
    // of the page. The blank that balanced the caption goes with it, or the
    // sentence would sit above centre for no visible reason.
    page.caption.set_visible(false);
    page.pad.set_visible(false);
    page.count.remove_css_class("tea-count");
    page.count.add_css_class("tea-await");
    page.count.set_justify(gtk::Justification::Center);

    // A clock is four characters wide and never changes; this is a sentence
    // somebody wrote, and it has to land on one line. Rather than wrapping it
    // at some fixed count of characters -- which broke "Scan the tag in the
    // living room" across two lines at thirty, and would break somebody else's
    // prompt at any other number -- the type is sized to the sentence and the
    // screen it is on.
    let width = page.monitor.geometry().width() as f64;
    let (size, fits) = ask_size(width, ask.chars().count());
    let attrs = gtk::pango::AttrList::new();
    attrs.insert(gtk::pango::AttrSize::new((size * gtk::pango::SCALE as f64) as i32));
    page.count.set_attributes(Some(&attrs));

    // If it fits at that size -- and after the sizing above it nearly always
    // does -- then nothing is allowed to break it. Wrapping stays off, and so
    // does ellipsizing: with either of them on, the label tells GTK it could
    // manage in less space, GTK believes it, and the sentence comes back in
    // two lines inside a box wide enough for one.
    let one_line = fits >= ask.chars().count() as i32;
    page.count.set_wrap(!one_line);
    page.count.set_ellipsize(match one_line {
        true => gtk::pango::EllipsizeMode::None,
        // A prompt somebody wrote a paragraph into is not going to fit on one
        // line at any size worth reading. Three lines of smaller type beats
        // one line of nothing.
        false => gtk::pango::EllipsizeMode::End,
    });
    page.count.set_max_width_chars(fits);
    page.count.set_lines(3);
    page.count.set_text(ask);

    page.sub.set_text("The page lifts the moment it hears from you.");
    // The time really is spent, so the ring stops being drawn -- see `Dial`.
    let mut dial = page.dial.borrow_mut();
    dial.remaining = 0.0;
    dial.spent = true;
}

/// What size to set a prompt of `chars` characters on a screen `width` wide,
/// and how many characters fit on a line at that size.
///
/// Split out from the widget so the arithmetic can be checked without a
/// display: this is the thing that decides whether the one sentence on the
/// page arrives whole.
fn ask_size(width: f64, chars: usize) -> (f64, i32) {
    // Monospaced, so every character is the same width: about 0.6 of the font
    // size in ems, and a point is four thirds of a pixel.
    const PER_CHAR: f64 = 0.6 * 4.0 / 3.0;
    // Not the whole screen: the sentence sits inside the same margins as
    // everything else on the page.
    let usable = width * 0.8;
    let chars = chars.max(1) as f64;
    let size = (usable / (chars * PER_CHAR)).clamp(ASK_SMALLEST, ASK_BIGGEST);
    let fits = (usable / (size * PER_CHAR)).floor().max(1.0);
    (size, fits as i32)
}

/// The prompt is set at this, and shrinks from here only if the sentence is
/// too long for the screen to take in one line.
const ASK_BIGGEST: f64 = 26.0;
/// Below this it stops being readable across a room, which is the whole point
/// of it, so a very long prompt wraps instead of shrinking further.
const ASK_SMALLEST: f64 = 13.0;

/// The walk paid off. On a page that was waiting this is what replaces the
/// asking -- for the second or so before the page comes down, which is exactly
/// long enough to see that it worked.
fn thank_them(page: &Page) {
    page.count.set_text("Off you go");
    page.sub.set_text("That's the break done.");
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
// A page is built from exactly this many things, and a struct to carry two of
// them across one call would be a struct with no other reason to exist.
#[allow(clippy::too_many_arguments)]
fn build_page(
    app: &gtk::Application,
    monitor: &gdk::Monitor,
    total: Duration,
    remaining: Duration,
    anim: &Anim,
    palette: &Palette,
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

    // Sized from the screen, not fixed: a slot generous enough to clear
    // the dial on a large display would not fit on a laptop panel at
    // all, and the column would be clipped.
    let geometry = monitor.geometry();
    // How many rows of squares the foot is carrying: a hundred-step walk is
    // two, and the slot below the clock has to be deep enough for them.
    let rows = face.walk.map_or(1, |walk| Meter::grid(walk.needed).1);
    let slot = slot_height(geometry.width(), geometry.height(), rows);
    let radius = ring_radius(geometry.width(), geometry.height());


    // The countdown must land on the exact centre of the screen, where
    // the dial is drawn. Rather than offsetting the other two from the
    // centre -- which left the subtitle sitting on top of the numbers --
    // the head and the foot are given equal fixed heights above and
    // below. Equal slots put the middle child in the middle by
    // construction, whatever the text in them turns out to be.
    let count = gtk::Label::new(Some(&clock(remaining)));
    count.add_css_class("tea-count");

    // The layers, sized from the monitor rather than from their own
    // allocation: the ring's box has to be built before it is measured. Built
    // after the label because the pace inside it is what moves the clock on.
    let (stage, state) = build_stage(total, remaining, arrival, anim, palette, radius, &count);

    // What the clock is a clock *of*, in the smallest type on the page. It
    // hangs below the numbers, inside the ring, and is balanced by an equal
    // blank above them -- otherwise the pair would centre itself and leave the
    // clock riding high in a ring drawn around the middle of the screen.
    let caption = gtk::Label::new(Some("REMAINING"));
    caption.add_css_class("tea-caption");
    caption.set_valign(gtk::Align::Start);
    let below = gtk::Box::new(gtk::Orientation::Vertical, 0);
    below.set_size_request(-1, CAPTION_SLOT);
    below.append(&caption);
    let above = gtk::Box::new(gtk::Orientation::Vertical, 0);
    above.set_size_request(-1, CAPTION_SLOT);

    let middle = gtk::Box::new(gtk::Orientation::Vertical, 0);
    middle.set_halign(gtk::Align::Center);
    middle.append(&above);
    middle.append(&count);
    middle.append(&below);

    let title = gtk::Label::new(Some("Time to stop"));
    title.add_css_class("tea-title");

    let sub = gtk::Label::new(Some(face.sub.as_deref().unwrap_or(SUB)));
    sub.add_css_class("tea-sub");
    // A prompt somebody wrote is longer than the line this ships with, and it
    // has to stay under the title rather than push the page wide.
    sub.set_wrap(true);
    sub.set_justify(gtk::Justification::Center);
    sub.set_max_width_chars(SUB_WIDTH);

    // The mark and the word, in a pill: enough branding for a page nobody
    // asked to see, and it doubles as the thing that says what this *is* to
    // somebody meeting it for the first time.
    let chip = gtk::Box::new(gtk::Orientation::Horizontal, 11);
    chip.set_halign(gtk::Align::Center);
    chip.add_css_class("tea-chip");
    chip.append(&logo(MARK));
    let word = gtk::Label::new(Some("BREAK"));
    word.add_css_class("tea-chip-word");
    chip.append(&word);

    // The chip, the title and the subtitle share the upper slot. They are
    // grouped and centred inside it rather than packed from its top edge, so
    // they stay balanced against the badges below.
    let group = gtk::Box::new(gtk::Orientation::Vertical, STACK_GAP);
    group.set_halign(gtk::Align::Center);
    group.set_valign(gtk::Align::Center);
    group.set_vexpand(true);
    group.append(&chip);
    group.append(&title);
    group.append(&sub);

    let head = gtk::Box::new(gtk::Orientation::Vertical, 0);
    head.set_size_request(-1, slot);
    head.append(&group);

    // The two halves of the gate, side by side in the order they happen in:
    // you scan on the way past, and the steps are what you do next.
    let badge = face.tag.map(|tag| Badge::new(Mark::Tag(tag)));
    let walk = face.walk.map(|walk| {
        Badge::new(match face.busted {
            true => Mark::Cheat(walk),
            false => Mark::Walk(walk),
        })
    });
    let moving = face.motion.map(|motion| Badge::new(Mark::Move(motion)));
    let badges = gtk::Box::new(gtk::Orientation::Horizontal, 14);
    badges.set_halign(gtk::Align::Center);
    if let Some(badge) = &badge {
        badges.append(&badge.row);
    }
    if let Some(walk) = &walk {
        badges.append(&walk.row);
    }
    if let Some(moving) = &moving {
        badges.append(&moving.row);
    }

    // And the walk again, as squares that fill up, fifty to a row: the badge
    // is the number, this is the picture of it, and the picture is the one you can
    // read from the doorway without your glasses on.
    let meter = face.walk.map(|walk| Meter::new(walk, *palette));

    let feet = gtk::Box::new(gtk::Orientation::Vertical, STACK_GAP + 4);
    feet.set_halign(gtk::Align::Center);
    feet.set_valign(gtk::Align::Center);
    feet.set_vexpand(true);
    feet.append(&badges);
    if let Some(meter) = &meter {
        feet.append(&meter.area);
    }

    let foot = gtk::Box::new(gtk::Orientation::Vertical, 0);
    foot.set_size_request(-1, slot);
    foot.append(&feet);

    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.append(&head);
    column.append(&middle);
    column.append(&foot);
    column.set_halign(gtk::Align::Center);
    column.set_valign(gtk::Align::Center);

    // Bottom to top: the dark, the ring around where the clock will be, the
    // blast over both, and the words over everything.
    let layers = gtk::Overlay::new();
    layers.set_child(Some(&stage.backdrop));
    layers.add_overlay(&stage.ring);
    layers.add_overlay(&stage.burst);
    layers.add_overlay(&column);
    win.set_child(Some(&layers));

    // The words arrive after the blast has passed over them. Every
    // timing is a share of the configured entrance, so turning that one
    // number changes the whole sequence in proportion.
    //
    // Fades only, no sliding: these three share a box, so animating a
    // margin would resize it every frame and the centred column -- the
    // countdown with it -- would twitch for the whole entrance.
    // The dark washes in by fading the layer, not by repainting it: the
    // backdrop is drawn once and this is the only thing that ever moves it.
    animate_in(&stage.backdrop, arrival * 0.18, 0, 0.0);
    animate_in(&middle, arrival * 0.34, 0, arrival * 0.30);
    animate_in(&head, arrival * 0.34, 0, arrival * 0.46);
    animate_in(&foot, arrival * 0.34, 0, arrival * 0.58);

    // Insurance: if the frame clock never delivers -- a stalled
    // compositor, a machine thrashing on resume -- an overlay stuck
    // part-way through would be an invisible break. Force the finished
    // state once the animation has had more than long enough.
    let settled: Vec<gtk::Widget> = vec![
        middle.clone().upcast(),
        head.clone().upcast(),
        foot.clone().upcast(),
        stage.backdrop.clone().upcast(),
    ];
    let finish = Rc::clone(&state);
    let redraw = stage.ring.clone();
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
    //
    // The size is asked for as well as the state, because fullscreen is a
    // request like any other: something that does not grant it leaves a window
    // at whatever GTK guesses, which is 200x200 and useless. Sized to the
    // monitor, the worst case is a page that covers the screen without being
    // flagged fullscreen, instead of a postage stamp in the corner.
    win.set_default_size(geometry.width(), geometry.height());
    win.fullscreen_on_monitor(monitor);
    win.present();

    let page = Page {
        win,
        monitor: monitor.clone(),
        count,
        caption: below,
        pad: above,
        title,
        sub,
        badge,
        walk,
        motion: moving,
        meter,
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
    palette: Palette,
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
                build_page(&app, &page.monitor, total, left, &anim, &palette, Entrance::None, &face)
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

/// How far apart the lines of the graph paper behind everything are.
const GRID: f64 = 72.0;

/// How long the confetti plays after a scan — and, when the scan is what ends
/// the break, how long the page stays up to play it: see `release`. Three
/// seconds: long enough to land as a reward, short enough that the desk it
/// just unlocked is not held hostage by its own applause.
const CELEBRATE: f64 = 3.0;

/// How tall the mark is drawn inside the chip at the top of the page. Fixed,
/// and small: it is a full stop next to the word BREAK, not a poster.
const MARK: i32 = 18;

/// The mark, drawn rather than scaled down from `assets/tea.png`.
///
/// The artwork is a full-colour illustration with its own outlines, and at
/// eighteen pixels beside a word in 10pt type it is a smudge -- the detail
/// that makes it a good icon at 512px is exactly what turns it to mush here.
/// One line and one arc survive the size, and they still read as a cup.
fn logo(height: i32) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.set_content_width(height);
    area.set_content_height(height);
    area.set_valign(gtk::Align::Center);
    area.set_draw_func(|_, cr, width, height| {
        let unit = width.min(height) as f64 / 18.0;
        cr.set_source_rgba(0.576, 0.639, 0.753, 1.0);
        cr.set_line_width(1.5 * unit);
        cr.set_line_cap(gtk::cairo::LineCap::Round);
        cr.set_line_join(gtk::cairo::LineJoin::Round);

        // The handle first, so the cup is drawn over where it meets the body.
        cr.new_path();
        cr.arc(13.0 * unit, 9.6 * unit, 2.6 * unit, -1.1, 1.1);
        let _ = cr.stroke();

        // The cup: straight sides that draw in a little, and a rounded base.
        cr.new_path();
        cr.move_to(3.0 * unit, 6.2 * unit);
        cr.line_to(12.4 * unit, 6.2 * unit);
        cr.line_to(11.4 * unit, 12.0 * unit);
        cr.curve_to(
            11.2 * unit,
            13.4 * unit,
            4.2 * unit,
            13.4 * unit,
            4.0 * unit,
            12.0 * unit,
        );
        cr.close_path();
        let _ = cr.stroke();
    });
    area
}

/// The dial's radius. The single number the whole layout is built from -- the
/// text slots are derived from it, so the two cannot disagree about where the
/// ring ends and the words begin.
///
/// The share is what a 720p panel can afford: on a short screen the slots get
/// capped by the screen rather than by the ring, and every pixel of radius
/// beyond this comes straight out of the air under the subtitle. See the
/// layout test, which is the thing that actually pins this number down.
fn ring_radius(width: i32, height: i32) -> f64 {
    (width.min(height) as f64 * 0.15).clamp(100.0, 200.0)
}

/// Clear air between the ring and the nearest line of text.
const RING_GAP: f64 = 56.0;

/// Rough heights of the stacks above and below the clock. Only estimates --
/// they decide how much air there is, never whether anything collides.
///
/// The middle is the clock plus the caption under it, and the caption is
/// balanced by an equal blank above, so the clock itself lands dead centre --
/// where the ring is drawn around it.
const COUNT_HEIGHT: f64 = 130.0;
/// The chip, the title and the subtitle, with the gaps between them.
const HEAD_HEIGHT: f64 = 140.0;
/// The two badges and one row of the step meter under them.
const FOOT_HEIGHT: f64 = 70.0;

/// The same, for a walk whose squares wrap onto `rows` of them. Every row past
/// the first makes the stack deeper, and a stack that grows without the slot
/// growing with it is a badge creeping back onto the ring.
fn foot_height(rows: u32) -> f64 {
    FOOT_HEIGHT + rows.saturating_sub(1) as f64 * (PIP + ROW_GAP)
}
/// The blank above the clock and the caption below it: equal, by construction.
const CAPTION_SLOT: i32 = 26;
/// Air between the pieces of the head and foot stacks.
const STACK_GAP: i32 = 14;

/// What the upper slot has to hold. Its *bottom* line -- the subtitle -- is
/// what reaches towards the ring, which is why the slot is sized from the whole
/// stack rather than from any one line of it.
fn head_height() -> f64 {
    HEAD_HEIGHT
}

/// Height reserved above and below the countdown, for the title and subtitle.
/// Equal by definition, which is what keeps the countdown centred, and derived
/// from the ring so the text always lands outside it.
///
/// Capped so the column can never be taller than the screen: GTK would squeeze
/// the slots to fit, dragging the text back inside the ring -- which is exactly
/// how it went wrong before.
fn slot_height(width: i32, height: i32, rows: u32) -> i32 {
    let radius = ring_radius(width, height);
    // Deep enough that the *bottom of the stack* clears the ring, not merely
    // the middle of the slot -- and sized to whichever stack is taller, since
    // both slots are the same depth and it is the deeper one that decides
    // whether anything lands on the ring.
    let stack = head_height().max(foot_height(rows));
    let clear_of_ring = 2.0 * (radius + RING_GAP) + stack - COUNT_HEIGHT;
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
    palette: &Palette,
    radius: f64,
    count: &gtk::Label,
) -> (Stage, Rc<RefCell<Dial>>) {
    let burst_share = anim.burst_share();
    let shards = anim.shard_count();
    let palette = *palette;

    let state = Rc::new(RefCell::new(Dial {
        total: total.as_secs_f64().max(1.0),
        remaining: remaining.as_secs_f64(),
        spent: false,
        // With the animation switched off, start already arrived.
        entrance: if arrival <= 0.0 { 1.0 } else { 0.0 },
        celebrate: None,
        last_frame: 0,
    }));

    // ---- the backdrop: drawn once, and then left alone ------------------
    //
    // Painted into a surface and blitted from there, because "drawn once" is
    // not something a draw func gets to decide. `animate_in` washes this layer
    // in by setting its opacity, and in GTK4 every opacity change queues
    // another draw -- so the half-second fade was re-rendering the whole thing
    // on every frame of itself, on every monitor at once. The glow is the
    // expensive part by three orders of magnitude: a full-screen radial
    // gradient costs 20ms at 1080p, 36ms at 1440p and 85ms at 4K, against a
    // 16.7ms frame. That is what made the entrance stutter, and only the
    // entrance -- nothing else on the page ever touches this layer again.
    let backdrop = gtk::DrawingArea::new();
    backdrop.set_hexpand(true);
    backdrop.set_vexpand(true);
    let cache: RefCell<Option<(i32, i32, gtk::cairo::ImageSurface)>> = RefCell::new(None);
    backdrop.set_draw_func(move |_, cr, width, height| {
        // Device pixels rather than logical ones: a cache built at logical
        // size comes back blurred on a scaled display.
        let (sx, sy) = cr.user_to_device_distance(1.0, 1.0).unwrap_or((1.0, 1.0));
        let (sx, sy) = (if sx > 0.0 { sx } else { 1.0 }, if sy > 0.0 { sy } else { 1.0 });
        let dw = ((width as f64 * sx).round() as i32).max(1);
        let dh = ((height as f64 * sy).round() as i32).max(1);

        let mut cache = cache.borrow_mut();
        if !matches!(&*cache, Some((w, h, _)) if *w == dw && *h == dh) {
            *cache = gtk::cairo::ImageSurface::create(gtk::cairo::Format::ARgb32, dw, dh)
                .ok()
                .and_then(|surface| {
                    let into = gtk::cairo::Context::new(&surface).ok()?;
                    into.scale(sx, sy);
                    paint_backdrop(&into, width, height, &palette);
                    Some((dw, dh, surface))
                });
        }

        match &*cache {
            Some((_, _, surface)) => {
                let _ = cr.save();
                cr.scale(1.0 / sx, 1.0 / sy);
                let _ = cr.set_source_surface(surface, 0.0, 0.0);
                let _ = cr.paint();
                let _ = cr.restore();
            }
            // A surface too large to allocate is no reason to show nothing.
            None => paint_backdrop(cr, width, height, &palette),
        }
    });

    // ---- the burst: the blast and the confetti, and nothing else --------
    let burst = gtk::DrawingArea::new();
    burst.set_hexpand(true);
    burst.set_vexpand(true);
    burst.set_can_target(false);
    let drawing = Rc::clone(&state);
    burst.set_draw_func(move |_, cr, width, height| {
        let dial = drawing.borrow();
        let entrance = dial.entrance;
        let (cx, cy) = (width as f64 / 2.0, height as f64 / 2.0);
        // Corner to corner, and a little past, so the blast leaves the page
        // rather than stopping short of it.
        let reach = (cx * cx + cy * cy).sqrt() * 1.12;
        cr.set_line_cap(gtk::cairo::LineCap::Round);

        // The blast, all the way out. Three waves rather than one: a single
        // expanding circle reads as a ripple, three read as a shock.
        let blast = phase(entrance, 0.02, burst_share.max(0.05));
        if burst_share > 0.0 && blast < 1.0 {
            for (start, weight, thickness) in [(0.0, 0.85, 10.0), (0.16, 0.55, 7.0), (0.34, 0.35, 5.0)] {
                let wave = phase(blast, start, 1.0);
                if wave <= 0.0 {
                    continue;
                }
                cr.set_line_width((thickness * (1.0 - wave)).max(0.5));
                let (r, g, b) = palette.wave;
                cr.set_source_rgba(r, g, b, (1.0 - wave).powi(2) * weight);
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
                let (r, g, b) = palette.light;
                cr.set_source_rgba(r, g, b, fade);
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
        if let Some(t) = dial.celebrate
            && t < 1.0
        {
            draw_celebration(cr, cx, cy, reach, t, &palette);
        }
    });

    // ---- the ring: a small box in the middle of the screen --------------
    // Big enough for the widest glow stroke and for the overshoot the arrival
    // ends on, and not one pixel bigger: this is the layer that gets redrawn
    // while the break runs, and its size is what that costs.
    let box_radius = radius * 1.10 + 12.0;
    let ring = gtk::DrawingArea::new();
    ring.set_content_width((box_radius * 2.0).ceil() as i32);
    ring.set_content_height((box_radius * 2.0).ceil() as i32);
    ring.set_halign(gtk::Align::Center);
    ring.set_valign(gtk::Align::Center);
    ring.set_can_target(false);
    let drawing = Rc::clone(&state);
    ring.set_draw_func(move |_, cr, width, height| {
        let dial = drawing.borrow();
        if dial.spent {
            return;
        }
        let (cx, cy) = (width as f64 / 2.0, height as f64 / 2.0);
        cr.set_line_cap(gtk::cairo::LineCap::Round);

        // The dial arrives in the middle, once the blast is on its way out.
        let arriving = phase(dial.entrance, 0.22, 0.80);
        if arriving <= 0.0 {
            return;
        }
        // Overshoot slightly at the end, so it lands rather than stops.
        let radius = radius * (0.25 + 0.75 * ease_out_back(arriving));

        // A hairline, not a hoop: the ring is a boundary drawn around the
        // clock, and the only heavy thing on this page should be the time.
        cr.set_line_width(2.5);
        let (r, g, b) = palette.hairline;
        cr.set_source_rgba(r, g, b, 0.9 * arriving);
        cr.arc(cx, cy, radius, 0.0, TAU);
        let _ = cr.stroke();

        // Time left, draining clockwise from the top. While it arrives, the arc
        // draws itself around rather than snapping to full.
        let drawn = arc_of(&dial) * ease_out_cubic(phase(dial.entrance, 0.42, 1.0));
        if drawn > 0.0 {
            // Three strokes, widest and faintest first: cairo has no blur, and
            // stacking them is what makes the arc look lit rather than painted.
            for (thickness, alpha) in [(16.0, 0.07), (8.0, 0.16), (3.0, 1.0)] {
                cr.set_line_width(thickness);
                let (r, g, b) = palette.accent;
                cr.set_source_rgba(r, g, b, alpha * arriving);
                cr.arc(cx, cy, radius, -FRAC_PI_2, -FRAC_PI_2 + TAU * drawn);
                let _ = cr.stroke();
            }
        }
    });

    // ---- what actually asks for a repaint, and how often ----------------
    let pace = Rc::new(Pace {
        dial: Rc::clone(&state),
        burst: burst.clone(),
        ring: ring.clone(),
        count: count.clone(),
        arrival,
        radius,
        painted: Cell::new(NEVER_PAINTED),
        shown: Cell::new(u64::MAX),
        ticking: Cell::new(false),
    });

    // The arrival is playing from the first frame, so the frame clock starts
    // with it. It takes itself off again the moment nothing needs it.
    pace.follow_the_frame_clock(&backdrop);

    // And underneath, a slow heartbeat for the rest of the break: it moves the
    // clock on, repaints the ring when its arc has actually gone somewhere, and
    // is what notices a celebration starting and calls the frame clock back.
    let weak = backdrop.downgrade();
    let beating = Rc::clone(&pace);
    glib::timeout_add_local(HEARTBEAT, move || {
        let Some(backdrop) = weak.upgrade() else {
            // The page is gone. So is the reason to keep waking up.
            return glib::ControlFlow::Break;
        };
        if beating.step() && !beating.ticking.get() {
            beating.follow_the_frame_clock(&backdrop);
        }
        glib::ControlFlow::Continue
    });

    (Stage { backdrop, burst, ring }, state)
}

/// The dark, the graph paper and the glow.
///
/// Split out from the draw func so it can be painted into a cache, used as the
/// fallback when one cannot be allocated, and timed without a display.
fn paint_backdrop(cr: &gtk::cairo::Context, width: i32, height: i32, palette: &Palette) {
    let (cx, cy) = (width as f64 / 2.0, height as f64 / 2.0);
    let reach = (cx * cx + cy * cy).sqrt();

    // Solid, or -- with `background = "dim"` -- most of the way there, so the
    // desk shows through as a shape rather than as something to read. The
    // window behind this is transparent either way; it is this layer that
    // decides how much of the desk survives.
    cr.set_source_rgba(0.024, 0.031, 0.051, palette.cover);
    let _ = cr.paint();

    // Graph paper, almost too faint to see: it gives the black somewhere to
    // be, so a screen that is entirely one colour does not read as a screen
    // that has died.
    cr.set_line_width(1.0);
    let (r, g, b) = palette.wave;
    cr.set_source_rgba(r, g, b, 0.022);
    let mut x = (width as f64 % GRID) / 2.0;
    while x < width as f64 {
        cr.move_to(x.floor() + 0.5, 0.0);
        cr.line_to(x.floor() + 0.5, height as f64);
        x += GRID;
    }
    let mut y = (height as f64 % GRID) / 2.0;
    while y < height as f64 {
        cr.move_to(0.0, y.floor() + 0.5);
        cr.line_to(width as f64, y.floor() + 0.5);
        y += GRID;
    }
    let _ = cr.stroke();

    // And a breath of light behind the dial, so the middle of the page is
    // where the eye goes. The costly line on this page: see `backdrop`.
    let glow = gtk::cairo::RadialGradient::new(cx, cy, 0.0, cx, cy, reach * 0.62);
    let (r, g, b) = palette.glow;
    glow.add_color_stop_rgba(0.0, r, g, b, 0.10);
    glow.add_color_stop_rgba(1.0, r, g, b, 0.0);
    let _ = cr.set_source(&glow);
    let _ = cr.paint();
}

/// How often the page is looked at when nothing is animating.
///
/// Eight times a second, which sounds slow for a countdown and is not: the ring
/// of a five-minute break creeps round at about three pixels a second, and the
/// numbers under it only change once a second anyway. What this is *not* is
/// sixty times a second for five minutes.
const HEARTBEAT: Duration = Duration::from_millis(120);

/// Who repaints what, and how often.
///
/// Two drivers share this: the frame clock, while something is genuinely
/// animating, and a slow timer for the rest of the break. Both go through
/// [`Pace::step`], and the elapsed time comes from the monotonic clock rather
/// than from either driver's own interval -- otherwise the two would both
/// advance the countdown and it would run at double speed whenever they
/// overlapped.
struct Pace {
    dial: Rc<RefCell<Dial>>,
    burst: gtk::DrawingArea,
    ring: gtk::DrawingArea,
    /// The countdown itself. Written from here rather than once a second by
    /// the host: see `step`.
    count: gtk::Label,
    arrival: f64,
    radius: f64,
    /// The arc as it was last actually painted, to compare against.
    painted: Cell<f64>,
    /// The whole second the label is currently showing. `u64::MAX` until it
    /// has written one, and only ever counts down -- see `step`.
    shown: Cell<u64>,
    /// Whether the frame clock is currently driving this.
    ticking: Cell<bool>,
}

impl Pace {
    /// Ask for every frame until nothing needs one.
    fn follow_the_frame_clock(self: &Rc<Self>, backdrop: &gtk::DrawingArea) {
        self.ticking.set(true);
        let pace = Rc::clone(self);
        backdrop.add_tick_callback(move |_, _| {
            if pace.step() {
                glib::ControlFlow::Continue
            } else {
                pace.ticking.set(false);
                glib::ControlFlow::Break
            }
        });
    }

    /// Move everything on to now, and repaint whatever that changed. Says
    /// whether anything is still animating.
    fn step(&self) -> bool {
        let now = glib::monotonic_time();
        let (animating, spent, arc, left) = {
            let mut dial = self.dial.borrow_mut();
            let delta = if dial.last_frame == 0 {
                0.0
            } else {
                (now - dial.last_frame) as f64 / 1_000_000.0
            };
            dial.last_frame = now;
            dial.entrance = if self.arrival <= 0.0 {
                1.0
            } else {
                (dial.entrance + delta / self.arrival).min(1.0)
            };
            dial.remaining = (dial.remaining - delta).max(0.0);
            if let Some(t) = dial.celebrate {
                dial.celebrate = Some((t + delta / CELEBRATE).min(1.0));
            }
            let playing = dial.entrance < 1.0 || dial.celebrate.is_some_and(|t| t < 1.0);
            (playing, dial.spent, arc_of(&dial), dial.remaining)
        };

        // The clock, moved on as soon as the second it shows has actually run
        // out. Written from here rather than once a second by the host,
        // because the host's tick and the second boundary drift against each
        // other: a label written only on the tick holds one number for two
        // seconds about once a break and skips another somewhere else, which
        // is the countdown visibly stalling. Looked at eight times a second,
        // every change lands within 125ms of the truth.
        //
        // Only ever downwards. The dial free-runs on the frame clock and is
        // corrected once a second from `/proc/uptime`, which lags by up to the
        // 10ms it is quantised to; without this the correction could nudge the
        // number back up for a single frame.
        if !spent {
            let want = left.max(0.0).ceil() as u64;
            if want < self.shown.get() {
                self.shown.set(want);
                self.count.set_text(&clock_secs(want));
            }
        }

        // The blast and the confetti live on a full-screen layer, so it is
        // hidden rather than merely left undrawn: a transparent layer the size
        // of the screen still costs something to composite, every frame, on
        // every monitor.
        if animating {
            if !self.burst.is_visible() {
                self.burst.set_visible(true);
            }
            self.burst.queue_draw();
        } else if self.burst.is_visible() {
            self.burst.set_visible(false);
        }

        if spent {
            // The countdown is over and the ring is not drawn any more. Take
            // the whole layer out rather than compositing an empty one.
            if self.ring.is_visible() {
                self.ring.set_visible(false);
            }
        } else if animating || worth_repainting(arc, self.painted.get(), self.radius) {
            self.painted.set(arc);
            self.ring.queue_draw();
        }

        animating
    }
}

/// What `Pace::painted` holds before the ring has been drawn even once.
///
/// Infinity rather than NaN, and the difference is the whole ring: the test
/// below is a comparison, every comparison against NaN is false, and `painted`
/// is only written *inside* the branch that comparison guards. Seeded with NaN
/// a page that is not animating on its first step never takes the branch, so
/// never seeds it, and never repaints the ring again -- which is every page
/// built with `Entrance::None` (a monitor plugged in mid-break, an insisting
/// page coming back) and every page at all with the entrance switched off.
const NEVER_PAINTED: f64 = f64::INFINITY;

/// Whether the arc has crept far enough since it was last painted to be worth
/// a frame -- three quarters of a pixel along its own circumference.
///
/// Split out from `Pace` so it can be checked without a display, because what
/// it answers on the *first* look is the difference between a ring that sweeps
/// for the whole break and one that stops dead a second in. See `painted`.
fn worth_repainting(arc: f64, painted: f64, radius: f64) -> bool {
    (arc - painted).abs() * TAU * radius >= 0.75
}

/// How much of the ring is still to be drawn, as a share of the whole.
fn arc_of(dial: &Dial) -> f64 {
    (dial.remaining / dial.total).clamp(0.0, 1.0)
}

/// Confetti colours, every one already on the page: the badge's green, the
/// dial's two blues, and the unseen badge's amber. A celebration in colours
/// the page has never used would look pasted on.
/// The green and the amber are fixed; the two blues follow the accent. See
/// [`Palette::confetti`].
const GREEN: (f64, f64, f64) = (0.451, 0.820, 0.620);
const AMBER: (f64, f64, f64) = (0.788, 0.639, 0.373);

/// The walk paid off: one green wave and a sky of confetti, launched from the
/// middle of the page and sinking as it fades. Varied without randomness, the
/// same way the shards are -- every scan earns the same celebration, and
/// nothing here needs a seed.
fn draw_celebration(
    cr: &gtk::cairo::Context,
    cx: f64,
    cy: f64,
    reach: f64,
    t: f64,
    palette: &Palette,
) {
    let confetti = palette.confetti();
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

        let (r, g, b) = confetti[(i % 4) as usize];
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

    /// The three cells a page is rebuilt from, with no walk in the break.
    fn faced(ask: Option<&str>, waiting: bool, scanned: bool, reachable: bool) -> Face {
        face_of(ask, waiting, scanned, reachable, waiting && scanned, Walk::default(), Motion::default(), false)
    }

    #[test]
    fn a_page_rebuilt_mid_break_remembers_the_walk() {
        const PROMPT: &str = "Scan the tag in the hall";
        let prompt = Some(PROMPT);

        // Nothing gating the break: no badge at all, on a page that has never
        // heard of a tag.
        assert!(faced(None, false, false, true).tag.is_none());
        assert!(faced(None, true, true, true).tag.is_none());

        // Gated: the badge tracks the scan, and survives whatever rebuilds the
        // page -- insisting, or a monitor arriving mid-break.
        assert_eq!(faced(prompt, false, false, true).tag, Some(Tag::Pending));
        assert_eq!(faced(prompt, false, true, true).tag, Some(Tag::Scanned));

        // Nothing watching: the page says so rather than asking for a walk that
        // would not be noticed. A scan already made outranks it.
        assert_eq!(faced(prompt, false, false, false).tag, Some(Tag::Unseen));
        assert_eq!(faced(prompt, false, true, false).tag, Some(Tag::Scanned));

        // Waiting *and* scanned is a page about to come down, and it says so
        // rather than still asking -- however many times it gets rebuilt in the
        // second before it goes.
        assert!(!faced(prompt, false, true, true).done, "not waiting yet");
        assert!(!faced(prompt, true, false, true).done, "waiting, nobody has been");
        assert!(faced(prompt, true, true, true).done);

        // The full waiting page only once the countdown is actually spent.
        assert_eq!(faced(prompt, false, false, true).ask, None);
        assert_eq!(faced(prompt, true, false, true).ask, Some(PROMPT.to_string()));
    }

    #[test]
    fn a_page_rebuilt_mid_walk_remembers_the_steps() {
        const PROMPT: &str = "Scan the tag in the hall";
        let prompt = Some(PROMPT);
        let part = Walk { walked: 12, needed: 20, marked: false };
        let full = Walk { walked: 20, needed: 20, marked: false };
        let face = |waiting, scanned, walk| face_of(prompt, waiting, scanned, true, false, walk, Motion::default(), false);

        // No walk in this break: no second badge, whatever else is going on.
        assert!(face(true, true, Walk::default()).walk.is_none());
        // With one, the count survives every rebuild -- and the page must never
        // come back at zero, which would read as steps that did not count.
        assert_eq!(face(false, false, part).walk, Some(part));
        assert_eq!(face(true, true, part).walk, Some(part));

        // Scanned, still walking: asking for the tag again would send somebody
        // back down the hall for a thing they have already done.
        assert_eq!(face(true, true, part).ask, Some("8 more steps".to_string()));
        assert_eq!(
            face(true, true, Walk { walked: 19, needed: 20, marked: false }).ask,
            Some("One more step".to_string())
        );
        // Not scanned: the tag is what is being waited for, steps or no steps.
        assert_eq!(face(true, false, part).ask, Some(PROMPT.to_string()));
        // Walked but not scanned, and the tag is still the ask.
        assert_eq!(face(true, false, full).ask, Some(PROMPT.to_string()));

        // Only the gate opening says the page is done -- not the walk on its
        // own, and not the scan on its own.
        assert!(!face(true, true, full).done, "the engine has not opened it yet");
        assert!(face_of(prompt, true, true, true, true, full, Motion::default(), false).done);
    }

    #[test]
    fn the_squares_wrap_at_fifty_and_a_square_stays_one_step() {
        // One square per step for any walk anybody actually asks for, so the
        // block is the count itself and not a proportion of it.
        assert_eq!(Meter::pips(50), 50);
        assert_eq!(Meter::grid(50), (50, 1));
        assert_eq!(Meter::grid(100), (50, 2));
        assert_eq!(Meter::grid(200), (50, 4));
        // A short last row, left-aligned under the full ones: 120 steps is two
        // full rows and twenty, not three rows of forty.
        assert_eq!(Meter::grid(120), (50, 3));
        assert_eq!(Meter::grid(20), (20, 1));
        assert_eq!(Meter::grid(1), (1, 1));
        // A walk of nought steps is not a walk, but the page must not be asked
        // to draw a block with no rows in it either.
        assert_eq!(Meter::grid(0), (1, 1));

        // Past four rows a square goes back to meaning a share of the walk,
        // because twenty rows of them is a texture rather than a count.
        assert_eq!(Meter::pips(1_000), PIPS_MOST);
        assert_eq!(Meter::grid(1_000), (50, 4));
    }

    #[test]
    fn no_two_badges_ever_read_the_same() {
        let words = |m| words_for(m);
        let all = [
            words(Mark::Tag(Tag::Pending)),
            words(Mark::Tag(Tag::Scanned)),
            words(Mark::Tag(Tag::Unseen)),
            words(Mark::Walk(Walk { walked: 0, needed: 20, marked: false })),
            words(Mark::Walk(Walk { walked: 0, needed: 20, marked: true })),
            words(Mark::Walk(Walk { walked: 20, needed: 20, marked: false })),
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
        // Green is for a half of the gate that is in, and nothing else.
        assert!(Mark::Walk(Walk { walked: 20, needed: 20, marked: false }).done());
        assert!(!Mark::Walk(Walk { walked: 19, needed: 20, marked: false }).done());
        assert!(!Mark::Tag(Tag::Unseen).done());
    }

    #[test]
    fn the_prompt_gets_one_line_of_its_own() {
        // The shipped prompt, and the one on this machine, on the smallest
        // screen anybody runs this on. Both have to arrive whole.
        for prompt in ["Scan the tag to get your desk back", "Scan the tag in the living room"] {
            let (size, fits) = ask_size(1280.0, prompt.chars().count());
            assert!(
                fits >= prompt.chars().count() as i32,
                "{prompt:?} wraps at {size}pt: {fits} characters fit of {}",
                prompt.chars().count()
            );
        }

        // A short one is not blown up past the size the page is designed at...
        assert_eq!(ask_size(3840.0, 12).0, ASK_BIGGEST);
        // ...and a long one shrinks to fit rather than wrapping.
        let (small, _) = ask_size(1280.0, 70);
        assert!(small < ASK_BIGGEST, "{small}pt");
        assert!(small >= ASK_SMALLEST);

        // Somebody's paragraph stops shrinking and wraps instead: unreadable
        // on one line is worse than readable on three.
        let (floored, fits) = ask_size(1280.0, 400);
        assert_eq!(floored, ASK_SMALLEST);
        assert!(fits > 1 && fits < 400);
    }

    #[test]
    fn the_ring_repaints_from_its_very_first_look() {
        const RADIUS: f64 = 150.0;

        // The seed has to read as "moved", because `painted` is only written
        // when this says so: a seed that answers no on the first look answers
        // no for ever, and the ring stops dead on every page born without an
        // entrance to play -- one rebuilt mid-break, or any page at all with
        // the animation switched off. NaN answers no to everything, which is
        // exactly how it went wrong.
        // Asserted against the seed a page is actually built with, not against
        // a value spelled out here -- the seed is the thing that was wrong.
        for arc in [0.0, 0.5, 1.0] {
            assert!(worth_repainting(arc, NEVER_PAINTED, RADIUS), "the first look must paint");
        }
        assert!(!worth_repainting(1.0, f64::NAN, RADIUS), "which is what NaN never did");

        // After that it is a real threshold: a hair of creep is not worth a
        // frame, a pixel of it is.
        assert!(!worth_repainting(0.5, 0.5, RADIUS));
        assert!(!worth_repainting(0.500_000_1, 0.5, RADIUS));
        assert!(worth_repainting(0.51, 0.5, RADIUS));
    }

    fn look(toml: &str) -> Look {
        toml::from_str(toml).unwrap()
    }

    #[test]
    fn the_page_ships_as_it_always_has() {
        let stock = Look::default();
        assert_eq!(stock.palette(), Palette::default());
        assert_eq!(stock.palette().cover, 1.0);
        assert!(stock.accent_misconfigured().is_none());
        assert!(!stock.prompts.on());
        // The stylesheet gets the same blue it was written with.
        let css = stock.css("rgba(ACCENT,0.1) MONO");
        assert!(css.starts_with("rgba(122,162,255,0.1) "));
        assert!(css.contains("JetBrains Mono"));
    }

    #[test]
    fn the_stock_accent_written_out_is_still_the_stock_palette() {
        // Case and spacing are not a departure from the default.
        assert_eq!(look(r##"accent = " #7AA2FF ""##).palette(), Palette::default());
    }

    #[test]
    fn another_accent_recolours_everything_that_is_not_text() {
        let warm = look(r##"accent = "#ff9966""##).palette();
        assert_ne!(warm, Palette::default());
        assert!((warm.accent.0 - 1.0).abs() < 1e-9);
        // Derived colours keep their relationships: the hairline is darker
        // than the arc, the shards lighter.
        assert!(warm.hairline.0 < warm.accent.0);
        assert!(warm.light.1 > warm.accent.1);
        let css = look(r##"accent = "#ff9966""##).css("ACCENT");
        assert_eq!(css, "255,153,102");
    }

    #[test]
    fn a_colour_that_is_not_one_costs_a_colour_and_not_a_break() {
        let odd = look(r#"accent = "blue""#);
        assert!(odd.accent_misconfigured().unwrap().contains("blue"));
        assert_eq!(odd.palette(), Palette::default());
        assert_eq!(odd.css("ACCENT"), "122,162,255");
    }

    #[test]
    fn dim_leaves_the_desk_showing_through() {
        let palette = look(r#"background = "dim""#).palette();
        assert!(palette.cover < 1.0 && palette.cover > 0.5);
    }

    #[test]
    fn a_font_goes_first_in_the_stack_and_cannot_break_out_of_its_quotes() {
        let css = look(r#"font = "IBM \"Plex\" Mono""#).css("MONO");
        assert!(css.starts_with("\"IBM Plex Mono\", \"JetBrains Mono\""));
    }

    #[test]
    fn prompts_read_as_a_switch_or_a_list() {
        assert_eq!(look(r#"prompts = "off""#).prompts, Prompts::Off);
        assert_eq!(look(r#"prompts = "on""#).prompts, Prompts::On);
        assert_eq!(look(r#"prompts = "enabled""#).prompts, Prompts::On);
        assert_eq!(
            look(r#"prompts = ["Water.", "  ", "Look up."]"#).prompts,
            Prompts::Custom(vec!["Water.".into(), "Look up.".into()])
        );
        // An empty list is off, written the long way.
        assert_eq!(look("prompts = []").prompts, Prompts::Off);
        assert!(toml::from_str::<Look>(r#"prompts = "sometimes""#).is_err());
        assert_eq!(Prompts::On.lines().len(), PROMPTS.len());
    }

    #[test]
    fn the_prompter_changes_its_line_on_the_beat_and_not_between() {
        let mut prompter =
            Prompter::new(&look(r#"prompts = ["a", "b", "c"]
prompt_every = "5s""#));
        prompter.begin_at(2);
        assert_eq!(prompter.current().as_deref(), Some("c"));
        for _ in 0..4 {
            assert_eq!(prompter.tick(), None);
        }
        // Wraps round to the start.
        assert_eq!(prompter.tick().as_deref(), Some("a"));
        assert_eq!(prompter.current().as_deref(), Some("a"));
        for _ in 0..4 {
            assert_eq!(prompter.tick(), None);
        }
        assert_eq!(prompter.tick().as_deref(), Some("b"));
    }

    #[test]
    fn the_prompter_says_nothing_when_there_is_nothing_to_say() {
        let mut off = Prompter::new(&Look::default());
        off.begin_at(3);
        assert_eq!(off.current(), None);
        for _ in 0..100 {
            assert_eq!(off.tick(), None);
        }
        // One line is shown and never swapped: a change to the same line is
        // a flicker for no reason.
        let mut one = Prompter::new(&look(r#"prompts = ["Water."]"#));
        one.begin_at(7);
        assert_eq!(one.current().as_deref(), Some("Water."));
        for _ in 0..100 {
            assert_eq!(one.tick(), None);
        }
    }

    #[test]
    fn a_silly_interval_is_clamped_rather_than_obeyed() {
        let quick = Prompter::new(&look(r#"prompts = "on"
prompt_every = "1s""#));
        assert_eq!(quick.every, 5);
        let slow = Prompter::new(&look(r#"prompts = "on"
prompt_every = "3h""#));
        assert_eq!(slow.every, 600);
    }

    #[test]
    fn the_moving_half_asks_for_its_own_thing_last() {
        let prompt = Some("Scan the tag");
        let walk = Walk { walked: 20, needed: 20, marked: false };
        let going = Motion { secs: 12, needed: 30, lost: false };
        // Countdown spent, tag in, walk in, phone still to say so.
        let face = face_of(prompt, true, true, true, false, walk, going, false);
        assert_eq!(face.ask.as_deref(), Some("Keep walking — 18s more"));
        assert_eq!(face.motion, Some(going));
        assert!(!face.done);
        // The walk outranks the moving: with steps still owed, that is what is
        // asked for, whatever the phone says.
        let short = Walk { walked: 5, needed: 20, marked: false };
        assert_eq!(
            face_of(prompt, true, true, true, false, short, going, false).ask.as_deref(),
            Some("15 more steps")
        );
        // A sensor that cannot be read is not asked for: the page falls back
        // to the tag's line and the badge says why.
        let lost = Motion { lost: true, ..going };
        assert_eq!(face_of(prompt, true, true, true, false, walk, lost, false).ask.as_deref(), Some("Scan the tag"));
        assert!(Mark::Move(lost).unseen());
        assert!(words_for(Mark::Move(lost)).contains("ends on the clock"));
        // Done: green, and it says so.
        let done = Motion { secs: 30, needed: 30, lost: false };
        assert!(Mark::Move(done).done());
        assert_eq!(words_for(Mark::Move(done)), "Moved");
        assert_eq!(words_for(Mark::Move(Motion { secs: 0, needed: 30, lost: false })), "Not moving yet");
        assert_eq!(words_for(Mark::Move(going)), "Moving · 12s of 30s");
        // Not part of this break: no badge at all.
        assert_eq!(face_of(prompt, false, false, true, false, walk, Motion::default(), false).motion, None);
    }

    #[test]
    fn a_shaken_phone_is_called_out_and_forgiven() {
        let prompt = Some("Scan the tag");
        let shook = Walk { walked: 27, needed: 100, marked: false };
        let none = Motion::default();
        // Countdown running: the badge teases, the ask is untouched.
        let face = face_of(prompt, false, false, true, false, shook, none, true);
        assert!(face.busted);
        assert_eq!(face.ask, None);
        // Countdown spent: the verdict is the big line, tag scanned or not.
        assert_eq!(face_of(prompt, true, false, true, false, shook, none, true).ask.as_deref(), Some(CAUGHT));
        assert_eq!(face_of(prompt, true, true, true, false, shook, none, true).ask.as_deref(), Some(CAUGHT));
        // Forgiven: back to counting.
        let face = face_of(prompt, true, true, true, false, shook, none, false);
        assert!(!face.busted);
        assert_eq!(face.ask.as_deref(), Some("73 more steps"));
        // No walk in this break: nothing to be caught at.
        assert!(!face_of(prompt, true, true, true, false, Walk::default(), none, true).busted);
        let mark = Mark::Cheat(shook);
        assert!(!mark.done());
        assert!(mark.unseen());
        assert_eq!(words_for(mark), "Nice try — 27 steps");
    }

    #[test]
    fn soft_is_the_default_and_does_not_insist() {
        assert!(!Hold::default().insists());
        assert!(Hold { mode: Grip::Insist, ..Hold::default() }.insists());
    }

    /// The layout has one job: the words go outside the ring, on every screen.
    #[test]
    fn the_text_always_clears_the_dial() {
        // Every depth of foot there can be: one row of squares, and up to the
        // four a two-hundred-step walk wraps onto.
        for rows in 1..=PIPS_MOST.div_ceil(PIPS_ROW) {
            for (w, h) in [(1280, 720), (1366, 768), (1920, 1080), (2560, 1440), (3840, 2160)] {
                let slot = slot_height(w, h, rows) as f64;
                let radius = ring_radius(w, h);

                // What matters is the edge nearest the ring, not the middle of
                // the slot it sits in: above, the subtitle at the bottom of the
                // chip - title - subtitle stack; below, the badges at the top
                // of theirs.
                let sub_bottom = COUNT_HEIGHT / 2.0 + slot / 2.0 - head_height() / 2.0;
                let badges_top = COUNT_HEIGHT / 2.0 + slot / 2.0 - foot_height(rows) / 2.0;

                // On a short screen the slot gets capped and the air narrows,
                // but it must never run out.
                for (what, edge) in [("subtitle", sub_bottom), ("badges", badges_top)] {
                    assert!(
                        edge >= radius + 24.0,
                        "{w}x{h}, {rows} rows: {what} reaches {edge:.0}px from centre, \
                         ring reaches {radius:.0}px"
                    );
                }

                // If the column outgrows the screen, GTK squeezes the slots and
                // the text lands back on the ring.
                assert!(
                    slot * 2.0 + COUNT_HEIGHT <= h as f64,
                    "{w}x{h}: column is {:.0}px on a {h}px screen",
                    slot * 2.0 + COUNT_HEIGHT
                );
            }
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

fn clock_secs(s: u64) -> String {
    format!("{}:{:02}", s / 60, s % 60)
}

/// A countdown, in whole seconds, rounded *up*.
///
/// Down was wrong at both ends. It showed `0:00` for the whole last second of
/// a break -- a clock that has plainly stopped above a page that is plainly
/// still up -- and it dropped a five-minute break straight from 5:00 to 4:58,
/// because the first tick lands a hair past the second and 298.99 floors to
/// 298. Rounding up, every number is on screen for the second it names.
fn clock(d: Duration) -> String {
    clock_secs(d.as_secs() + u64::from(d.subsec_nanos() > 0))
}


#[cfg(test)]
mod paint_tests {
    use super::*;

    fn surface(w: i32, h: i32) -> gtk::cairo::ImageSurface {
        gtk::cairo::ImageSurface::create(gtk::cairo::Format::ARgb32, w, h).unwrap()
    }

    /// What the fade was costing, and what it costs now.
    ///
    /// `animate_in` sets the backdrop's opacity once a frame, and every
    /// opacity change queues a draw -- so this ran for every frame of the wash
    /// in, on every monitor. Against a 16.7ms frame it was not close.
    #[test]
    fn painting_the_backdrop_costs_more_than_a_frame() {
        let s = surface(1920, 1080);
        let cr = gtk::cairo::Context::new(&s).unwrap();
        paint_backdrop(&cr, 1920, 1080, &Palette::default()); // warm

        let t = std::time::Instant::now();
        for _ in 0..5 {
            paint_backdrop(&cr, 1920, 1080, &Palette::default());
        }
        s.flush();
        let each = t.elapsed().as_secs_f64() / 5.0;

        // Not a threshold anybody has to keep green -- it is the reason the
        // cache exists, asserted so that a future "just draw it again" has to
        // argue with a number.
        assert!(
            each > 0.008,
            "the backdrop got cheap enough to redraw ({:.1}ms) -- the cache may be moot",
            each * 1000.0
        );
    }

    /// Blitting the cache is what the fade actually pays now.
    #[test]
    fn blitting_the_cache_fits_in_a_frame() {
        let (w, h) = (1920, 1080);
        let cached = surface(w, h);
        {
            let into = gtk::cairo::Context::new(&cached).unwrap();
            paint_backdrop(&into, w, h, &Palette::default());
        }
        let target = surface(w, h);
        let cr = gtk::cairo::Context::new(&target).unwrap();
        let _ = cr.set_source_surface(&cached, 0.0, 0.0);
        let _ = cr.paint();

        let t = std::time::Instant::now();
        for _ in 0..20 {
            let _ = cr.set_source_surface(&cached, 0.0, 0.0);
            let _ = cr.paint();
        }
        target.flush();
        let each = t.elapsed().as_secs_f64() / 20.0;

        assert!(
            each < 0.0167 / 2.0,
            "a blit has to leave most of the frame for everything else: {:.2}ms",
            each * 1000.0
        );
    }

    /// The countdown, driven the way `Pace::step` drives it, against the way
    /// the host used to.
    ///
    /// The host ticks on its own clock and the second boundary moves on
    /// another; sampling the label only on the tick means the two drift
    /// through each other, and every time they cross, a number is either held
    /// for two seconds or skipped entirely. That is the countdown stalling.
    #[test]
    fn the_clock_never_holds_a_number_or_skips_one() {
        // A five-minute break, a host tick a hair over a second, and a
        // boottime clock quantised to the 10ms `/proc/uptime` reports in.
        const TOTAL: f64 = 300.0;
        const TICK: f64 = 1.0004;

        // How it was: the label written once per host tick, rounding down.
        let mut old_seq = vec![TOTAL.floor() as u64];
        let (mut t, mut prev_q, mut rem) = (0.0f64, 0.0f64, TOTAL);
        while rem > 0.0 {
            t += TICK;
            let q = (t * 100.0).floor() / 100.0;
            rem -= q - prev_q;
            prev_q = q;
            if rem <= 0.0 {
                break;
            }
            old_seq.push(rem.floor() as u64);
        }

        // How it is: the dial free-runs, the host corrects it once a tick, and
        // the label is looked at eight times a second and only ever counts
        // down -- exactly what `Pace::step` does.
        let mut shown = u64::MAX;
        let mut new_seq: Vec<u64> = Vec::new();
        let (mut prev_q, mut sched) = (0.0f64, TOTAL);
        let mut dial = TOTAL;
        let mut next_tick = TICK;
        let mut now = 0.0f64;
        while now < TOTAL + 1.0 {
            now += 1.0 / 8.0;
            dial = (dial - 1.0 / 8.0).max(0.0);
            if now >= next_tick {
                let q = (next_tick * 100.0).floor() / 100.0;
                next_tick += TICK;
                sched -= q - prev_q;
                prev_q = q;
                if sched <= 0.0 {
                    break;
                }
                dial = sched; // the once-a-second correction
            }
            let want = dial.max(0.0).ceil() as u64;
            if want < shown {
                shown = want;
                new_seq.push(want);
            }
        }

        let holds = |v: &[u64]| v.windows(2).filter(|w| w[0] == w[1]).count();
        let skips = |v: &[u64]| v.windows(2).filter(|w| w[0] - w[1] > 1).count();

        // The old way stumbles; that is the reported symptom.
        assert!(
            holds(&old_seq) + skips(&old_seq) > 0,
            "the old sampling was supposed to stumble: {:?}",
            &old_seq[..12.min(old_seq.len())]
        );

        // The new way does not, and still counts every number exactly once.
        assert_eq!(holds(&new_seq), 0, "a number was held for two seconds");
        assert_eq!(skips(&new_seq), 0, "a number was skipped");
        assert!(new_seq.windows(2).all(|w| w[0] - w[1] == 1), "every step is one second");
    }

    /// A countdown rounds up: the last second of a break reads 0:01, not a
    /// clock that has stopped at 0:00 above a page that is plainly still up.
    #[test]
    fn the_last_second_of_a_break_is_not_zero() {
        assert_eq!(clock(Duration::from_millis(1)), "0:01");
        assert_eq!(clock(Duration::from_millis(999)), "0:01");
        assert_eq!(clock(Duration::ZERO), "0:00", "and nothing left really is nothing");

        // The first tick of a five-minute break lands a hair past the second;
        // rounding down turned that into a break that opens 5:00 -> 4:58.
        assert_eq!(clock(Duration::from_secs_f64(298.996)), "4:59");
        assert_eq!(clock(Duration::from_secs(300)), "5:00");
        assert_eq!(clock(Duration::from_secs(90)), "1:30");
    }

    /// The cache has to be pixel-for-pixel what the draw func would have put
    /// there, or the fix is a redesign wearing a performance hat.
    #[test]
    fn the_cache_is_what_the_draw_func_would_have_drawn() {
        let (w, h) = (321, 197); // deliberately not round
        let direct = surface(w, h);
        {
            let cr = gtk::cairo::Context::new(&direct).unwrap();
            paint_backdrop(&cr, w, h, &Palette::default());
        }

        let cached = surface(w, h);
        {
            let into = gtk::cairo::Context::new(&cached).unwrap();
            paint_backdrop(&into, w, h, &Palette::default());
        }
        let blitted = surface(w, h);
        {
            let cr = gtk::cairo::Context::new(&blitted).unwrap();
            let _ = cr.set_source_surface(&cached, 0.0, 0.0);
            let _ = cr.paint();
        }

        let a = direct.take_data().unwrap();
        let b = blitted.take_data().unwrap();
        assert_eq!(a.len(), b.len());
        assert!(a.iter().eq(b.iter()), "the blit is not the same picture");
    }
}

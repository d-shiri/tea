//! GTK4 soft-enforcement overlay for GNOME/Wayland.
//!
//! "Soft" is a deliberate limit, not an oversight: Wayland has no equivalent of
//! `XGrabKeyboard`, and Mutter implements no layer-shell protocol, so a normal
//! client cannot take the seat. What it *can* do is cover every monitor, refuse
//! to close, and shove itself back in front when you switch away. Anyone
//! determined can still escape; the goal is to make ignoring it deliberate.

use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use crate::config::Dur;
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
    windows: Vec<gtk::ApplicationWindow>,
    counters: Vec<gtk::Label>,
    dials: Vec<Rc<RefCell<Dial>>>,
    warning: Option<gtk::ApplicationWindow>,
}

impl GtkBlocker {
    pub fn new(app: &gtk::Application, postpone: Rc<Cell<bool>>, anim: Anim) -> Self {
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
            windows: Vec::new(),
            counters: Vec::new(),
            dials: Vec::new(),
            warning: None,
        }
    }

    fn monitors() -> Vec<gdk::Monitor> {
        let Some(display) = gdk::Display::default() else {
            return Vec::new();
        };
        let list = display.monitors();
        (0..list.n_items())
            .filter_map(|i| list.item(i).and_then(|o| o.downcast::<gdk::Monitor>().ok()))
            .collect()
    }
}

impl Blocker for GtkBlocker {
    fn engage(&mut self, total: Duration) {
        self.release();
        println!("\n[BREAK] stop. {} of rest.", clock(total));

        let arrival = self.anim.seconds();
        let monitors = Self::monitors();
        for (index, monitor) in monitors.iter().enumerate() {
            let win = gtk::ApplicationWindow::builder()
                .application(&self.app)
                .decorated(false)
                .title("tea")
                .build();
            win.add_css_class("tea-overlay");

            // One drawing surface covering the whole window. It has to be the
            // full page: cairo clips to the widget, so a blast drawn inside a
            // small dial can never reach the edges of the screen.
            let (stage, state) = build_stage(total, &self.anim);

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
            let count = gtk::Label::new(Some(&clock(total)));
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
            let settled: Vec<gtk::Widget> = vec![
                count.clone().upcast(),
                head.clone().upcast(),
                sub.clone().upcast(),
            ];
            let finish = Rc::clone(&state);
            let redraw = stage.clone();
            glib::timeout_add_local_once(
                Duration::from_secs_f64((arrival * 1.6).max(1.0)),
                move || {
                    for w in &settled {
                        w.set_opacity(1.0);
                    }
                    finish.borrow_mut().entrance = 1.0;
                    redraw.queue_draw();
                },
            );

            // Escape, Alt-F4 and the rest go nowhere. The break ends when the
            // break ends.
            let keys = gtk::EventControllerKey::new();
            keys.connect_key_pressed(|_, _, _, _| glib::Propagation::Stop);
            win.add_controller(keys);
            win.connect_close_request(|_| glib::Propagation::Stop);

            // Only one window chases focus. If every monitor's window did, they
            // would each steal it from the next and spin forever.
            if index == 0 {
                win.connect_is_active_notify(|w| {
                    if !w.is_active() && w.is_visible() {
                        w.present();
                    }
                });
            }

            win.fullscreen_on_monitor(monitor);
            win.present();
            self.counters.push(count);
            self.dials.push(state);
            self.windows.push(win);
        }

        if self.windows.is_empty() {
            eprintln!("tea: no monitors found — overlay not shown");
        }
    }

    fn update(&mut self, remaining: Duration) {
        let text = clock(remaining);
        for label in &self.counters {
            label.set_text(&text);
        }
        // The dial runs itself between ticks so the sweep is smooth; this is
        // the once-a-second correction back to what the scheduler actually says.
        for dial in &self.dials {
            dial.borrow_mut().remaining = remaining.as_secs_f64();
        }
    }

    fn release(&mut self) {
        if self.windows.is_empty() {
            return;
        }
        for win in self.windows.drain(..) {
            // close_request is wired to Stop, so ask the window to go away in a
            // way it cannot veto.
            win.destroy();
        }
        self.counters.clear();
        self.dials.clear();
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
const LOGO: &[u8] = include_bytes!("../assets/tea.png");

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
fn build_stage(total: Duration, anim: &Anim) -> (gtk::DrawingArea, Rc<RefCell<Dial>>) {
    let arrival = anim.seconds();
    let burst_share = anim.burst_share();
    let shards = anim.shard_count();

    let state = Rc::new(RefCell::new(Dial {
        total: total.as_secs_f64().max(1.0),
        remaining: total.as_secs_f64(),
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

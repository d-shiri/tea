//! P0 host: drives the scheduler on a real clock and prints what it would do.
//! No overlay yet — this exists to shake out the timing before any GTK lands.

mod config;
mod overlay;
mod session;
mod settings;
mod sound;
mod state;
mod status;

use config::human;
use gtk::glib;
use gtk::prelude::*;
use tea_core::{Blocker, PostponeResult, Scheduler};
use overlay::{GtkBlocker, TerminalBlocker};
use session::Session;
use std::cell::{Cell, RefCell};
use std::io::Read;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

const TICK: Duration = Duration::from_secs(1);
const APP_ID: &str = "dev.tea.Tea";

#[derive(Default)]
struct Overrides {
    work: Option<Duration>,
    brk: Option<Duration>,
    warn_before: Option<Duration>,
    idle_credit: Option<Duration>,
    idle_pause: Option<Duration>,
    postpone: Option<Duration>,
    postpone_budget: Option<u32>,
}

fn main() {
    // Rust ignores SIGPIPE so that writes report errors rather than killing the
    // process. Every `println!` then panics when a pipe closes, which turns
    // `tea status | head` into a backtrace. Restore the default disposition:
    // for a command-line tool, dying quietly when the reader goes away is right.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };

    let mut over = Overrides::default();
    let mut explicit_path: Option<PathBuf> = None;
    let mut write_only = false;
    let mut headless = false;
    let mut reload = false;
    let mut run_page = false;
    let mut run_warning = false;
    let mut run_for: Option<Duration> = None;
    let mut probe = false;
    let mut show_status = false;
    let mut show_config = false;
    let mut set_key: Option<(&str, String)> = None;
    let mut set_sound: Option<String> = None;

    // Indexed rather than an iterator so a command can look at the argument
    // after it without swallowing it: `tea run` and `tea run 5s` are both
    // valid, and `tea run --headless` must not read the flag as a length.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "-w" | "--work" => {
                over.work = Some(dur_at(&argv, i, "--work"));
                i += 1;
            }
            "-b" | "--break" => {
                over.brk = Some(dur_at(&argv, i, "--break"));
                i += 1;
            }
            "--warn-before" => {
                over.warn_before = Some(dur_at(&argv, i, "--warn-before"));
                i += 1;
            }
            "--idle-credit" => {
                over.idle_credit = Some(dur_at(&argv, i, "--idle-credit"));
                i += 1;
            }
            "--idle-pause" => {
                over.idle_pause = Some(dur_at(&argv, i, "--idle-pause"));
                i += 1;
            }
            "--postpone" => {
                over.postpone = Some(dur_at(&argv, i, "--postpone"));
                i += 1;
            }
            "--postpone-budget" => {
                let raw = value_at(&argv, i, "--postpone-budget");
                over.postpone_budget =
                    Some(raw.parse().unwrap_or_else(|_| fail("--postpone-budget needs a number")));
                i += 1;
            }
            "--headless" => headless = true,
            "--probe" => probe = true,
            "reload" => reload = true,
            "status" => show_status = true,
            "config" => show_config = true,
            "run" | "--test-overlay" => {
                run_page = true;
                if let Some(length) = argv.get(i + 1).and_then(|v| config::parse(v)) {
                    run_for = Some(length);
                    i += 1;
                }
            }
            "run-warning" | "--test-warning" => {
                run_warning = true;
                if let Some(length) = argv.get(i + 1).and_then(|v| config::parse(v)) {
                    run_for = Some(length);
                    i += 1;
                }
            }
            "set-work" => {
                set_key = Some(("work", value_at(&argv, i, "set-work")));
                i += 1;
            }
            "set-break" => {
                set_key = Some(("break", value_at(&argv, i, "set-break")));
                i += 1;
            }
            "set-warn" => {
                set_key = Some(("warn_before", value_at(&argv, i, "set-warn")));
                i += 1;
            }
            "set-sound" => {
                set_sound = Some(value_at(&argv, i, "set-sound"));
                i += 1;
            }
            "-c" | "--config" => {
                explicit_path = Some(PathBuf::from(value_at(&argv, i, "--config")));
                i += 1;
            }
            "--write-config" => write_only = true,
            "-h" | "--help" => return usage(),
            other => fail(&format!("unknown argument {other:?} (try --help)")),
        }
        i += 1;
    }

    let path = explicit_path.clone().or_else(config::default_path).unwrap_or_else(|| {
        fail("cannot locate a config directory: set XDG_CONFIG_HOME or HOME, or pass --config")
    });

    if reload {
        return run_reload();
    }

    if let Some(file) = set_sound {
        if let Err(e) = config::write_default(&path) {
            fail(&e);
        }
        match settings::set_sound(&path, std::path::Path::new(&file)) {
            Ok(change) => {
                println!("tea: {change}");
                println!("tea: run `tea reload` to apply");
            }
            Err(e) => fail(&e),
        }
        return;
    }

    if let Some((key, value)) = set_key {
        // Create the file first if it is missing: there has to be something to
        // edit, and the starter file is what they would get on first run anyway.
        if let Err(e) = config::write_default(&path) {
            fail(&e);
        }
        match settings::set(&path, key, &value) {
            Ok(change) => {
                println!("tea: {change}");
                println!("tea: run `tea reload` to apply");
            }
            Err(e) => fail(&e),
        }
        return;
    }

    // A named file that is not there is a typo, wherever it is named. The
    // daemon path already refuses it; these two were quietly showing defaults
    // instead, which reads as "your settings are gone".
    if (show_config || show_status) && explicit_path.is_some() && !path.exists() {
        fail(&format!("no config at {}", path.display()));
    }

    if show_config {
        let file = if path.exists() {
            config::load(&path).unwrap_or_else(|e| fail(&e))
        } else {
            println!("tea: no config file yet — showing the defaults");
            config::FileConfig::default()
        };
        let mut cfg: tea_core::Config = file.clone().into();
        let _ = config::reconcile(&mut cfg);
        return settings::show(&cfg, &file, &path);
    }

    if show_status {
        let mut cfg = if path.exists() {
            config::load(&path).unwrap_or_else(|e| fail(&e)).into()
        } else {
            tea_core::Config::default()
        };
        let _ = config::reconcile(&mut cfg);
        return status::show(cfg, &path, boottime());
    }

    // An explicit --config that isn't there is a typo, not an invitation to
    // create one somewhere the user didn't mean.
    if explicit_path.is_none() || write_only {
        match config::write_default(&path) {
            Ok(true) => println!("tea: wrote a starter config to {}", path.display()),
            Ok(false) if write_only => {
                println!("tea: {} already exists, leaving it alone", path.display());
            }
            Ok(false) => {}
            Err(e) => fail(&e),
        }
    }
    if write_only {
        return;
    }

    let file = if path.exists() {
        config::load(&path).unwrap_or_else(|e| fail(&e))
    } else {
        fail(&format!("no config at {} (--write-config creates one)", path.display()))
    };

    let sound_cfg = file.sound.clone();
    let anim_cfg = file.animation.clone();
    let mut cfg: tea_core::Config = file.into();
    let Overrides { work, brk, warn_before, idle_credit, idle_pause, postpone, postpone_budget } =
        over;
    if let Some(v) = work {
        cfg.work = v;
    }
    if let Some(v) = brk {
        cfg.brk = v;
    }
    if let Some(v) = warn_before {
        cfg.warn_before = v;
    }
    if let Some(v) = idle_credit {
        cfg.idle_credit = v;
    }
    if let Some(v) = idle_pause {
        cfg.idle_pause = v;
    }
    if let Some(v) = postpone {
        cfg.postpone = v;
    }
    if let Some(v) = postpone_budget {
        cfg.postpone_budget = v;
    }
    for note in config::reconcile(&mut cfg).unwrap_or_else(|e| fail(&e)) {
        eprintln!("tea: note: {note}");
    }

    if probe {
        return run_probe();
    }

    if run_page {
        // Default to a real break, so what you see is what you will get.
        let total = run_for.unwrap_or(cfg.brk);
        println!("tea: showing the break page for {}", human(total));
        return preview(total, sound_cfg, anim_cfg);
    }

    if run_warning {
        let total = run_for.unwrap_or(cfg.warn_before);
        println!("tea: showing the warning for {}", human(total));
        return preview_warning(total, cfg.postpone, anim_cfg);
    }

    println!(
        "tea: work {}, break {}. Idle {}+ counts as a break.\n\
         config: {}\nCtrl-C to stop.",
        human(cfg.work),
        human(cfg.brk),
        human(cfg.idle_credit),
        path.display()
    );

    if headless {
        run_headless(cfg, sound_cfg);
    } else {
        run_gtk(cfg, sound_cfg, anim_cfg);
    }
}

/// Terminal-only: no GTK, no display needed. Works over SSH.
fn run_headless(cfg: tea_core::Config, sound: sound::Config) {
    let mut engine = Engine::start(cfg, sound);
    let mut ui = TerminalBlocker;
    loop {
        std::thread::sleep(TICK);
        engine.step(&mut ui);
    }
}

/// The real thing. GTK owns the main loop; the engine rides a 1s timeout on it,
/// so there are no threads and no locking anywhere in this program.
fn run_gtk(cfg: tea_core::Config, sound: sound::Config, anim: overlay::Anim) {
    let app = gtk::Application::builder().application_id(APP_ID).build();

    let started = Cell::new(false);
    app.connect_activate(move |app| {
        // A second `tea` does not start its own process: GTK routes it to
        // this one as another activation. Without this guard that starts a
        // SECOND engine inside the running service -- two timers, two of every
        // window, one second apart.
        if started.replace(true) {
            eprintln!("tea: already running; ignoring the second start");
            return;
        }

        // Nothing is on screen between breaks, and GtkApplication quits when
        // its last window closes -- so hold it open explicitly.
        let hold = app.hold();
        let engine = RefCell::new(Engine::start(cfg.clone(), sound.clone()));
        let ui =
            RefCell::new(GtkBlocker::new(app, engine.borrow().postpone_flag(), anim.clone()));

        glib::timeout_add_seconds_local(1, move || {
            let _keep = &hold;
            engine.borrow_mut().step(&mut *ui.borrow_mut());
            glib::ControlFlow::Continue
        });
    });

    // We parse our own flags; hand GTK only the program name.
    let argv: Vec<String> = std::env::args().take(1).collect();
    app.run_with_args(&argv);
}

/// Put the overlay up for a fixed time and exit, so it can be tried without
/// waiting out a work interval.
fn preview(total: Duration, sound: sound::Config, anim: overlay::Anim) {
    // Deliberately not APP_ID: sharing it would route this into the running
    // service instead of starting a throwaway app.
    let app = gtk::Application::builder()
        .application_id("dev.tea.Preview")
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(move |app| {
        let ui = Rc::new(RefCell::new(GtkBlocker::new(
            app,
            Rc::new(Cell::new(false)),
            anim.clone(),
        )));
        let mut player = sound::Player::new(sound.clone());
        player.break_starts();
        ui.borrow_mut().engage(total);

        let left = Cell::new(total);
        let app = app.clone();
        glib::timeout_add_seconds_local(1, move || {
            let remaining = left.get().saturating_sub(TICK);
            left.set(remaining);
            if remaining.is_zero() {
                player.break_ends();
                ui.borrow_mut().release();
                app.quit();
                return glib::ControlFlow::Break;
            }
            ui.borrow_mut().update(remaining);
            glib::ControlFlow::Continue
        });
    });
    let argv: Vec<String> = std::env::args().take(1).collect();
    app.run_with_args(&argv);
}

/// Show the warning toast on its own, so its wording and behaviour can be
/// checked without sitting through a work interval.
fn preview_warning(total: Duration, postpone: Duration, anim: overlay::Anim) {
    let app = gtk::Application::builder()
        .application_id("dev.tea.Preview")
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(move |app| {
        let ui = Rc::new(RefCell::new(GtkBlocker::new(
            app,
            Rc::new(Cell::new(false)),
            anim.clone(),
        )));
        ui.borrow_mut().warn(total, Some(tea_core::Snooze { duration: postpone, left: 2 }));

        let left = Cell::new(total);
        let app = app.clone();
        glib::timeout_add_seconds_local(1, move || {
            let remaining = left.get().saturating_sub(TICK);
            left.set(remaining);
            if remaining.is_zero() {
                ui.borrow_mut().clear_warning();
                app.quit();
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
    });
    let argv: Vec<String> = std::env::args().take(1).collect();
    app.run_with_args(&argv);
}

/// Ties the scheduler to the clock, the session bus and the state file. Both
/// front ends drive this, so they cannot drift apart.
struct Engine {
    sched: Scheduler,
    session: Session,
    store: Option<state::Store>,
    last: Duration,
    /// Downtime to account for on the first step after a restart.
    catchup: Option<(Duration, Duration)>,
    postpone: Rc<Cell<bool>>,
    sound: sound::Player,
}

impl Engine {
    fn start(cfg: tea_core::Config, sound: sound::Config) -> Self {
        let now = boottime();
        let store = state::Store::new();
        let restored = store.as_ref().and_then(|s| s.load(now));

        let (sched, catchup) = match restored {
            Some(r) => {
                println!(
                    "tea: resuming — {} banked, away {} ({})",
                    human(r.snapshot.worked),
                    human(r.gap),
                    if r.idle.is_zero() { "counted as work" } else { "counted as rest" }
                );
                (Scheduler::restore(cfg, r.snapshot), Some((r.gap, r.idle)))
            }
            None => (Scheduler::new(cfg), None),
        };

        Self {
            sched,
            session: Session::connect(),
            store,
            last: now,
            catchup,
            postpone: Rc::new(Cell::new(false)),
            sound: sound::Player::new(sound),
        }
    }

    fn postpone_flag(&self) -> Rc<Cell<bool>> {
        Rc::clone(&self.postpone)
    }

    /// Say that a break has been held up, and by what. Deliberately only says
    /// it: forcing a break through would put the overlay over a live call,
    /// which is the one thing this must never do.
    fn report_overdue(&mut self, waiting: Duration) {
        let held_by = match self.session.inhibitors().as_slice() {
            [] => "something that did not name itself".to_string(),
            names => names.join(", "),
        };
        println!("[held]  break waiting {} — held by {held_by}", human(waiting));
        self.session.notify(
            "Break overdue",
            &format!(
                "A break has been waiting {} — held by {held_by}. \
                 Nothing has been interrupted.",
                human(waiting)
            ),
        );
    }

    fn step(&mut self, ui: &mut dyn Blocker) {
        if self.postpone.replace(false) {
            match self.sched.postpone() {
                PostponeResult::Granted { remaining_budget } => {
                    println!("[snooze] postponed, {remaining_budget} left this window");
                    ui.clear_warning();
                }
                other => println!("[snooze] refused: {other:?}"),
            }
        }

        let now = boottime();
        let (delta, idle, inhibited) = match self.catchup.take() {
            // First step after a restart: settle the downtime before the clock
            // starts ticking normally.
            Some((gap, idle)) => (gap, idle, false),
            None => {
                let delta = now.saturating_sub(self.last);
                // A gap in our own ticks means suspend, or a stopped process.
                // Keep it even though the compositor can be asked directly: its
                // idle clock resets the instant you type your unlock password,
                // which would otherwise erase an hour of genuine rest.
                let gap = delta.saturating_sub(TICK * 2);
                let idle = self.session.idle().unwrap_or(Duration::ZERO).max(gap);
                (delta, idle, self.session.inhibited())
            }
        };
        self.last = now;

        let before = self.sched.snapshot();
        let commands = tea_core::drive(&mut self.sched, ui, delta, idle, inhibited);
        let after = self.sched.snapshot();

        for cmd in &commands {
            match *cmd {
                tea_core::Command::Overdue { waiting } => self.report_overdue(waiting),
                tea_core::Command::Warn { until_break } => {
                    // The overlay only opens a window when there is a postpone
                    // to click. With nothing to act on, a notification says the
                    // same thing without taking over the screen.
                    if self.sched.postpones_left() == 0 {
                        self.session.notify(
                            "Break soon",
                            &format!("Starting in {}.", human(until_break)),
                        );
                    }
                }
                tea_core::Command::ShowOverlay { .. } => self.sound.break_starts(),
                tea_core::Command::HideOverlay => self.sound.break_ends(),
                _ => {}
            }
        }

        if let Some(store) = &mut self.store {
            // Anything that would hurt to lose gets written immediately; plain
            // accumulation rides the throttle.
            let notable = before.breaking != after.breaking
                || before.postpones_used != after.postpones_used
                || after.worked < before.worked;
            store.save(after, now, notable);
        }
    }
}

/// Print what the session reports, once a second, so idle detection can be
/// eyeballed without waiting out a work interval.
fn run_probe() {
    let mut session = Session::connect();
    println!("tea: probing the session bus (5s). Stop touching the keyboard.");
    for _ in 0..5 {
        std::thread::sleep(TICK);
        let idle = match session.idle() {
            Some(d) => format!("{:>6}ms", d.as_millis()),
            None => "     ?".to_string(),
        };
        let holding = match session.inhibitors().as_slice() {
            [] => "nothing".to_string(),
            names => names.join(", "),
        };
        println!("idle {idle}   break held by: {holding}");
    }
    println!(
        "\nIf you run this during a call and it still says \"nothing\", your call app\n\
         does not tell the system it is busy, and tea cannot see the call."
    );
}

/// Restart the background service so edited settings take effect. Safe at any
/// moment: break debt is persisted, so it resumes rather than resetting.
fn run_reload() {
    match std::process::Command::new("systemctl")
        .args(["--user", "restart", "tea.service"])
        .status()
    {
        Ok(status) if status.success() => println!("tea: reloaded — settings are live"),
        Ok(_) => fail(
            "systemctl could not restart tea.service — is it installed? (./dist/install.sh sets it up)",
        ),
        Err(e) => fail(&format!("cannot run systemctl: {e}")),
    }
}

fn value_at(argv: &[String], i: usize, what: &str) -> String {
    argv.get(i + 1).cloned().unwrap_or_else(|| fail(&format!("{what} needs a value")))
}

fn dur_at(argv: &[String], i: usize, what: &str) -> Duration {
    let raw = value_at(argv, i, what);
    config::parse(&raw)
        .unwrap_or_else(|| fail(&format!("{what}: {raw:?} is not a duration like 25m")))
}

/// CLOCK_BOOTTIME via /proc/uptime: unlike `Instant` (CLOCK_MONOTONIC on Linux)
/// it keeps counting across suspend, which is exactly the time we want to see.
fn boottime() -> Duration {
    let mut buf = String::new();
    std::fs::File::open("/proc/uptime")
        .and_then(|mut f| f.read_to_string(&mut buf))
        .unwrap_or_else(|e| fail(&format!("cannot read /proc/uptime: {e}")));
    let secs: f64 = buf
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| fail("unexpected /proc/uptime format"));
    Duration::from_secs_f64(secs)
}

fn usage() {
    // Laid out from a table rather than padded by hand: the hand-padded version
    // had descriptions starting in three different columns.
    const COMMANDS: &[(&str, &str)] = &[
        ("run [dur]", "show the break page (defaults to your break)"),
        ("run-warning [dur]", "show the warning toast"),
        ("status", "what the background service is doing"),
        ("config", "show every setting"),
        ("reload", "pick up changed settings"),
        ("set-work <dur>", "change the work interval"),
        ("set-break <dur>", "change the break length"),
        ("set-warn <dur>", "change the warning time"),
        ("set-sound <file>", "play this file when a break starts"),
    ];
    const OPTIONS: &[(&str, &str)] = &[
        ("-c, --config <path>", "use this config file instead"),
        ("-w, --work <dur>", "work interval"),
        ("-b, --break <dur>", "break length"),
        ("    --warn-before <dur>", "heads-up before a break"),
        ("    --idle-credit <dur>", "idle time that counts as a break"),
        ("    --idle-pause <dur>", "idle time that stops the work timer"),
        ("    --postpone <dur>", "time one postpone buys"),
        ("    --postpone-budget <n>", "postpones allowed per window"),
        ("    --headless", "terminal only, no GTK window"),
        ("    --probe", "print idle/inhibitor readings and exit"),
        ("    --write-config", "create the starter config and exit"),
    ];

    let width = COMMANDS
        .iter()
        .chain(OPTIONS)
        .map(|(flag, _)| flag.chars().count())
        .max()
        .unwrap_or(0)
        + 2;

    println!("tea — take a break, whether you like it or not\n");
    println!("Usage:\n  tea [command] [options]\n");

    println!("Commands:");
    for (name, what) in COMMANDS {
        println!("  {name:<width$}{what}");
    }

    println!("\nOptions (these override the config file):");
    for (flag, what) in OPTIONS {
        println!("  {flag:<width$}{what}");
    }

    println!("\nSettings live in ~/.config/tea/config.toml, or $XDG_CONFIG_HOME/tea/.");
    println!("Durations: 90s, 25m, 1h. A bare number means minutes.");
}

fn fail(msg: &str) -> ! {
    eprintln!("tea: {msg}");
    std::process::exit(2)
}

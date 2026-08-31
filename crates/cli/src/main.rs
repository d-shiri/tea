//! P0 host: drives the scheduler on a real clock and prints what it would do.
//! No overlay yet — this exists to shake out the timing before any GTK lands.

mod config;
mod nfc;
mod overlay;
mod session;
mod settings;
mod sound;
mod state;
mod status;

use config::human;
use gtk::glib;
use gtk::prelude::*;
use tea_core::{Blocker, PostponeResult, ReleaseResult, Scheduler};
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
    let mut set_nfc: Option<String> = None;
    let mut unlock = false;

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
            "set-nfc" => {
                set_nfc = Some(value_at(&argv, i, "set-nfc"));
                i += 1;
            }
            "unlock" => unlock = true,
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

    if let Some(switch) = set_nfc {
        if let Err(e) = config::write_default(&path) {
            fail(&e);
        }
        match settings::set_nfc(&path, &switch) {
            Ok(change) => {
                println!("tea: {change}");
                // Read it back rather than reporting what we meant to write:
                // the URL printed here is the one that has to work.
                if let Ok(file) = config::load(&path)
                    && file.nfc.on()
                {
                    tag_instructions(&file.nfc);
                }
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
    let hold_cfg = file.hold;
    let nfc_cfg = file.nfc.clone();
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

    if unlock {
        if !nfc_cfg.on() {
            fail("nfc is off in this config — `tea set-nfc on` first");
        }
        return match nfc::knock(&nfc_cfg) {
            Ok(reply) => println!("tea: {reply}"),
            Err(e) => fail(&e),
        };
    }

    if probe {
        return run_probe(&nfc_cfg);
    }

    if run_page {
        // Default to a real break, so what you see is what you will get.
        let total = run_for.unwrap_or(cfg.brk);
        println!("tea: showing the break page for {}", human(total));
        if nfc_cfg.on() {
            println!(
                "tea: the tag is on — after the countdown the page waits for a real scan,\n\
                 \x20    exactly like a break would."
            );
        }
        return preview(total, sound_cfg, anim_cfg, hold_cfg, nfc_cfg);
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

    if nfc_cfg.on() {
        println!(
            "nfc: on — the page waits for the tag, {}",
            match cfg.release_grace {
                g if g.is_zero() => "for as long as it takes".to_string(),
                g => format!("giving up after {}", human(g)),
            }
        );
    }

    if headless {
        run_headless(cfg, sound_cfg, nfc_cfg);
    } else {
        run_gtk(cfg, sound_cfg, anim_cfg, hold_cfg, nfc_cfg);
    }
}

/// What to write on the tag, and where to hold your phone.
fn tag_instructions(cfg: &nfc::Config) {
    let host = cfg.listen.split(':').next().unwrap_or("");
    let port = cfg.listen.rsplit(':').next().unwrap_or("9797");
    let token = &cfg.token;

    // Nothing to write on the tag at all: Home Assistant owns it, and the app
    // that scanned it is the app that wrote it.
    if cfg.asks() {
        let ha = &cfg.home_assistant;
        println!("tea: the tag is Home Assistant's — {}", ha.entity);
        println!("     write the tag from the companion app: Settings → Tags");
        println!("tea: tea asks {} about it while a break is up, so nothing", ha.url);
        println!("     on this machine listens and nothing has to reach it.");
        return;
    }

    // Something else is the front door, so where tea listens is nobody's
    // business but the proxy's -- and none of the advice below applies.
    if cfg.fronted() {
        println!("tea: write this URL on the tag:");
        println!("       {}", cfg.tag_url());
        println!("     (tea itself listens on {} — nothing on this machine", cfg.listen);
        println!("      is reachable from the network)");
        return;
    }

    // "listen on everything" is an instruction to this machine, not an address
    // anything can dial. Printing it on the tag line would be printing a URL
    // that cannot work.
    if host == "0.0.0.0" || host == "::" {
        match nfc::lan_address() {
            Some(ip) => {
                println!("tea: write this URL on the tag:");
                println!("       http://{ip}:{port}/unlock?token={token}");
                println!(
                    "     (tea answers on every address this machine has; that is the one\n\
                     \x20     your phone can reach, as long as it is on the same network)"
                );
            }
            None => {
                println!("tea: write this on the tag, with this machine's address in place of HOST:");
                println!("       http://HOST:{port}/unlock?token={token}");
            }
        }
        return;
    }

    println!("tea: write this URL on the tag:");
    println!("       {}", cfg.tag_url());
    if host.starts_with("127.") || host == "localhost" || host == "::1" {
        println!(
            "tea: note — nfc.listen is loopback, so only this machine can reach it.\n\
             \x20    A phone in another room needs either listen = \"0.0.0.0:{port}\"\n\
             \x20    and a hole in the firewall, or nfc.url pointing at something\n\
             \x20    that is already listening. See \"The ear\" in the README."
        );
    }
}

/// Terminal-only: no GTK, no display needed. Works over SSH.
fn run_headless(cfg: tea_core::Config, sound: sound::Config, nfc: nfc::Config) {
    let mut engine = Engine::start(cfg, sound, nfc);
    let mut ui = TerminalBlocker;
    // There is no GTK main loop out here, but the socket that listens for the
    // tag still dispatches on glib's. Pumping whatever is pending each second
    // is enough for a doorbell: the answer is built from the last tick anyway.
    let context = glib::MainContext::default();
    loop {
        std::thread::sleep(TICK);
        while context.pending() {
            context.iteration(false);
        }
        engine.step(&mut ui);
    }
}

/// The real thing. GTK owns the main loop; the engine rides a 1s timeout on it,
/// so there are no threads and no locking anywhere in this program.
fn run_gtk(
    cfg: tea_core::Config,
    sound: sound::Config,
    anim: overlay::Anim,
    hold: overlay::Hold,
    nfc: nfc::Config,
) {
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
        // its last window closes -- so keep it open explicitly.
        let keep_open = app.hold();
        let engine = RefCell::new(Engine::start(cfg.clone(), sound.clone(), nfc.clone()));
        let ui = RefCell::new(GtkBlocker::new(
            app,
            engine.borrow().postpone_flag(),
            anim.clone(),
            hold,
            nfc.on().then(|| nfc.prompt.clone()),
        ));

        glib::timeout_add_seconds_local(1, move || {
            let _keep = &keep_open;
            engine.borrow_mut().step(&mut *ui.borrow_mut());
            glib::ControlFlow::Continue
        });
    });

    // We parse our own flags; hand GTK only the program name.
    let argv: Vec<String> = std::env::args().take(1).collect();
    app.run_with_args(&argv);
}

/// Put the overlay up for one break and exit, so it can be tried without
/// waiting out a work interval. With the tag on, the same ear and the same
/// watch the daemon uses are wired in, and the page waits for a *real* scan —
/// a dry run that pretends the walk was made proves nothing about the tag.
fn preview(
    total: Duration,
    sound: sound::Config,
    anim: overlay::Anim,
    hold: overlay::Hold,
    nfc: nfc::Config,
) {
    /// Only when nothing real can hear a scan does the preview act one out:
    /// this long on the waiting page, so it can be looked at...
    const LOOK_AT_IT_FOR: Duration = Duration::from_secs(6);
    /// ...and how much of that is spent *after* the scan — pretend or real.
    /// Long enough for the whole celebration. The order matters: waiting
    /// first, then scanned, then the page lifts, because that is the only
    /// order a real break can happen in. Flipping the badge green while the
    /// page is still asking shows a state that cannot exist, and teaches you
    /// to distrust the badge.
    const THEN_LIFT_AFTER: Duration = Duration::from_secs(3);

    // Deliberately not APP_ID: sharing it would route this into the running
    // service instead of starting a throwaway app.
    let app = gtk::Application::builder()
        .application_id("dev.tea.Preview")
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(move |app| {
        let asking = nfc.on();
        let link = nfc::Link::new();

        // Hear a real scan every way the daemon can. Neither failure is fatal
        // here: the running service usually owns the port, and the whole point
        // of a preview is seeing what there is to see.
        let mut ear = None;
        let mut watch = None;
        if asking {
            match nfc::listen(&nfc, Rc::clone(&link)) {
                Ok(e) => {
                    println!("nfc: listening on {}", e.addr);
                    ear = Some(e);
                }
                // Only worth a line when the ear was the only way a scan
                // could have arrived.
                Err(why) if !nfc.asks() => eprintln!("tea: {why}"),
                Err(_) => {}
            }
            if nfc.asks() {
                match nfc::watch(&nfc.home_assistant, Rc::clone(&link)) {
                    Ok(w) => {
                        println!(
                            "nfc: watching {} — scan the tag to lift the page",
                            nfc.home_assistant.entity.trim()
                        );
                        watch = Some(w);
                    }
                    Err(why) => eprintln!("tea: {why}"),
                }
            }
            if ear.is_none() && watch.is_none() {
                println!("nfc: nothing can hear a real scan, so the preview will act one out");
            }
        }
        let wired = ear.is_some() || watch.is_some();

        let ui = Rc::new(RefCell::new(GtkBlocker::new(
            app,
            Rc::new(Cell::new(false)),
            anim.clone(),
            hold,
            asking.then(|| nfc.prompt.clone()),
        )));
        let mut player = sound::Player::new(sound.clone());
        player.break_starts();
        ui.borrow_mut().engage(total);

        let grace = nfc.grace.0;
        let left = Cell::new(total);
        let waited = Cell::new(Duration::ZERO);
        let shown = Cell::new(Duration::ZERO);
        let asked = Cell::new(false);
        let scanned = Cell::new(false);
        let late = Cell::new(false);
        let app = app.clone();
        glib::timeout_add_seconds_local(1, move || {
            // Held here so the ear keeps listening and the watch keeps polling
            // for as long as the page is up.
            let _keep = (&ear, &watch);

            // A scan lands between ticks, exactly as it does in the daemon.
            // Early ones bank: the countdown still runs out.
            if link.take_scan() && !scanned.replace(true) {
                println!("[scan]  the tag was scanned");
                late.set(asked.get());
                ui.borrow_mut().release_seen();
                player.scanned();
            }

            let remaining = left.get().saturating_sub(TICK);
            left.set(remaining);

            let done = if !remaining.is_zero() {
                ui.borrow_mut().update(remaining);
                false
            } else if !asking {
                true
            } else if scanned.get() {
                // A scan that ended the wait gets a moment on screen before
                // the page lifts — flashing straight past "scanned" reads as a
                // glitch, not a walk that registered.
                if late.get() {
                    shown.set(shown.get() + TICK);
                    shown.get() >= THEN_LIFT_AFTER
                } else {
                    true
                }
            } else {
                if !asked.replace(true) {
                    ui.borrow_mut().await_release();
                }
                waited.set(waited.get() + TICK);
                if wired {
                    // The daemon's rules, not softer ones: a watcher that
                    // cannot be reached, or a grace that runs out, ends the
                    // break on the clock and says so.
                    let lost = link.reachable() == Some(false);
                    ui.borrow_mut().release_source(!lost);
                    let gave_up = !grace.is_zero() && waited.get() >= grace;
                    if lost {
                        println!("[ha]    nothing to ask — ending on the clock");
                    } else if gave_up {
                        println!("[wait]  no scan after {} — giving up", human(waited.get()));
                    }
                    lost || gave_up
                } else {
                    // Nothing is listening, so the scan is acted out instead.
                    if waited.get() + THEN_LIFT_AFTER >= LOOK_AT_IT_FOR && !scanned.replace(true) {
                        println!("[scan]  (preview) somebody pretends to scan the tag");
                        late.set(true);
                        ui.borrow_mut().release_seen();
                        player.scanned();
                    }
                    false
                }
            };

            if done {
                // A scan that ended the wait already had its chime; the
                // end-of-break sound on top would be a clatter.
                if !late.get() {
                    player.break_ends();
                }
                ui.borrow_mut().release();
                app.quit();
                return glib::ControlFlow::Break;
            }

            // What the watch's "only while a break is up" gate reads.
            link.post(nfc::Desk {
                breaking: true,
                remaining,
                waiting: asked.get() && !scanned.get(),
                released: scanned.get(),
            });
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
        // The toast never holds the screen, so how the break page behaves does
        // not come into it.
        let ui = Rc::new(RefCell::new(GtkBlocker::new(
            app,
            Rc::new(Cell::new(false)),
            anim.clone(),
            overlay::Hold::default(),
            None,
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
    /// Where the tag's scans land, and where the tick leaves the answer the
    /// server gives out. Held even when nfc is off, so the tick has one shape.
    link: Rc<nfc::Link>,
    /// The listening socket, alive only as long as the engine is.
    _ear: Option<nfc::Ear>,
    /// The poll that asks Home Assistant about the tag, same lifetime.
    _watch: Option<nfc::Watch>,
}

impl Engine {
    fn start(cfg: tea_core::Config, sound: sound::Config, nfc_cfg: nfc::Config) -> Self {
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

        let link = nfc::Link::new();
        // A port that will not open must not stop the timer: the break page
        // still works, the grace still ends it, and the reason is on stderr.
        let ear = nfc_cfg.on().then(|| nfc::listen(&nfc_cfg, Rc::clone(&link))).and_then(|r| {
            match r {
                Ok(ear) => {
                    println!("nfc: listening on {}", ear.addr);
                    Some(ear)
                }
                Err(e) => {
                    eprintln!("tea: nfc is off — {e}");
                    None
                }
            }
        });

        // Asking is the tidier half of this: nothing has to be forwarded in, and
        // an unreachable hub is a thing tea finds out about by itself.
        let watch = nfc_cfg.asks().then(|| nfc::watch(&nfc_cfg.home_assistant, Rc::clone(&link)))
            .and_then(|r| match r {
                Ok(watch) => {
                    println!(
                        "nfc: watching {} on {}",
                        nfc_cfg.home_assistant.entity, nfc_cfg.home_assistant.url
                    );
                    Some(watch)
                }
                Err(e) => {
                    // Configured and broken is worse than not configured: the
                    // gate would be on with nothing able to open it. Say the
                    // source is unreachable, which is what it is, and let the
                    // page and the countdown deal with it honestly.
                    eprintln!("tea: not watching Home Assistant — {e}");
                    link.set_reachable(false);
                    None
                }
            });

        Self {
            sched,
            session: Session::connect(),
            store,
            last: now,
            catchup,
            postpone: Rc::new(Cell::new(false)),
            sound: sound::Player::new(sound),
            link,
            _ear: ear,
            _watch: watch,
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
        // The server never touches the scheduler; this is where a scan that
        // arrived between ticks actually lands.
        let mut celebrated = false;
        if self.link.take_scan() {
            match self.sched.released() {
                ReleaseResult::Freed => {
                    println!("[nfc]   tag scanned — the desk is yours");
                    // Told now, not via the next snapshot: the break resets on
                    // this very tick, and a page torn down without ever
                    // acknowledging the walk reads as a scan that was
                    // swallowed. This is also what starts the celebration.
                    ui.release_seen();
                    self.sound.scanned();
                    celebrated = true;
                }
                ReleaseResult::Banked { remaining } => {
                    println!("[nfc]   tag scanned — {} of the break still to run", human(remaining));
                    ui.release_seen();
                    self.sound.scanned();
                }
                ReleaseResult::NotBreaking => println!("[nfc]   tag scanned — no break to end"),
                ReleaseResult::NotRequired => println!("[nfc]   tag scanned — nothing was waiting"),
            }
        }

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
                // The scan that just freed this break has its own sound; the
                // ordinary end-of-break one stacked on top would turn the
                // celebration into a clatter.
                tea_core::Command::HideOverlay if !celebrated => self.sound.break_ends(),
                tea_core::Command::HideOverlay => {}
                _ => {}
            }
        }

        // Told every tick, not just the one the scan landed on: a page that is
        // rebuilt -- or resumed after a restart -- has to come back knowing the
        // walk was already made. The blocker does the work once.
        if after.breaking && after.released {
            ui.release_seen();
        }

        if after.breaking && !after.released {
            // `None` means nothing has looked yet, which is not the same as
            // "cannot be reached" and must not paint the page as broken.
            let lost = self.link.reachable() == Some(false);
            ui.release_source(!lost);

            // A gate nobody can open is not a gate, it is a lock. If the thing
            // that would notice a scan cannot be reached when the countdown
            // runs out, the break ends on the clock and says why. Degrading is
            // the rule everywhere else in here; there is no reason for the one
            // feature that holds your screen to be the exception.
            if lost && self.sched.awaiting_release() {
                self.sched.released();
                println!("[ha]    nothing to ask — ending the break on the clock");
            }
        }

        self.link.post(nfc::Desk {
            breaking: after.breaking,
            remaining: self.sched.config().brk.saturating_sub(after.rested),
            waiting: self.sched.awaiting_release(),
            released: after.released,
        });

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
fn run_probe(nfc_cfg: &nfc::Config) {
    if nfc_cfg.asks() {
        let ha = &nfc_cfg.home_assistant;
        print!("tea: asking {} about {} ... ", ha.url, ha.entity);
        match nfc::probe(ha) {
            Ok(state) => println!("{state:?}"),
            Err(e) => println!("\ntea: {e}"),
        }
        println!();
    }

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
        ("set-nfc on|off", "hold the page until a tag is scanned"),
        ("unlock", "scan the tag from here, without the tag"),
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

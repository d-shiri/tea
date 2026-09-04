//! P0 host: drives the scheduler on a real clock and prints what it would do.
//! No overlay yet — this exists to shake out the timing before any GTK lands.

mod clock;
mod config;
mod dash;
mod history;
mod nfc;
mod overlay;
mod publish;
mod session;
mod settings;
mod sound;
mod state;
mod status;
mod strict;
mod web;

use config::human;
use gtk::glib;
use gtk::prelude::*;
use tea_core::{Blocker, PostponeResult, ReleaseResult, Scheduler};
use overlay::{GtkBlocker, TerminalBlocker};
use serde_json::json;
use session::Session;
use std::cell::{Cell, RefCell};
use std::io::Read;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

const TICK: Duration = Duration::from_secs(1);
const APP_ID: &str = "dev.tea.Tea";
/// Steps that have to arrive with the phone saying `still` throughout before
/// the page calls it a hand rather than a walk. Fewer than the shortest walk
/// anyone configures, more than a phone picked up off the desk earns.
const CHEAT_STEPS: u32 = 20;
/// And how long the phone has to have said nothing about moving first: its
/// activity sensor is a minute behind at times, and an accusation that arrives
/// before the sensor does is the page calling a walker a liar.
const CHEAT_AFTER: Duration = Duration::from_secs(30);

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
    let mut show_dash = false;
    let mut open_dash = true;
    let mut open_settings = false;
    /// The default `tea off` with no duration named. Long enough to be worth
    /// asking for, short enough that forgetting to say `tea on` costs you one
    /// afternoon rather than the habit.
    const OFF_FOR: Duration = Duration::from_secs(60 * 60);
    let mut off_for: Option<Duration> = None;
    let mut back_on = false;
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
            "off" => {
                // A bare `tea off` is an hour. Anything that parses as a
                // duration after it is taken as one, so `tea off 20m` reads
                // the way it looks.
                off_for = Some(match argv.get(i + 1) {
                    Some(v) => {
                        let d = config::parse(v).unwrap_or_else(|| {
                            fail(&format!("off: {v:?} is not a duration like 1h or 20m"))
                        });
                        i += 1;
                        d
                    }
                    None => OFF_FOR,
                });
            }
            "on" => back_on = true,
            "status" => show_status = true,
            "config" => show_config = true,
            "dash" => show_dash = true,
            "settings" => open_settings = true,
            "--no-open" => open_dash = false,
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

    // Neither of these touches the config file or needs the daemon restarted:
    // the running service reads the switch on its next tick, which is within a
    // second, and the page comes down with it.
    if let Some(how_long) = off_for {
        return match state::off::set(how_long) {
            Ok(()) => {
                println!(
                    "tea: off for {} — nothing will interrupt you until then.",
                    human(how_long)
                );
                println!("     `tea on` starts it again sooner.");
            }
            Err(e) => fail(&e),
        };
    }
    if back_on {
        return match (state::off::left(), state::off::clear()) {
            (_, Err(e)) => fail(&e),
            (Some(left), Ok(())) => {
                println!("tea: back on, {} early. The work timer starts from here.", human(left))
            }
            (None, Ok(())) => println!("tea: already on."),
        };
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
    if (show_config || show_status || show_dash || open_settings)
        && explicit_path.is_some()
        && !path.exists()
    {
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

    // Reads what is already on disk and writes a file of its own. No GTK, no
    // daemon, and no starter config written as a side effect of wanting to look
    // at a chart.
    if show_dash {
        let file = if path.exists() {
            config::load(&path).unwrap_or_else(|e| fail(&e))
        } else {
            config::FileConfig::default()
        };
        let mut cfg: tea_core::Config = file.clone().into();
        let _ = config::reconcile(&mut cfg);
        return dash::show(&file, &cfg, &path, boottime(), open_dash);
    }

    // Opens a browser at the running daemon's page. No daemon of its own: the
    // page is served by the service, and this only knows where.
    if open_settings {
        if !path.exists() {
            fail(&format!("no config at {} (--write-config creates one)", path.display()));
        }
        return web::open(&path);
    }

    if show_status {
        let file = if path.exists() {
            config::load(&path).unwrap_or_else(|e| fail(&e))
        } else {
            config::FileConfig::default()
        };
        let hours = file.hours.clone();
        let ignore = file.calls.ignore.clone();
        let mut cfg: tea_core::Config = file.into();
        let _ = config::reconcile(&mut cfg);
        return status::show(cfg, &hours, &ignore, &path, boottime());
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
    let look_cfg = file.page.clone();
    let mut nfc_cfg = file.nfc.clone();
    let hours_cfg = file.hours.clone();
    let calls_cfg = file.calls.clone();
    let site = file.settings.on().then(|| web::Site { path: path.clone() });
    // A page switched on by hand, in a file that has never had a token: mint
    // one now rather than refuse the port and send somebody to read about
    // tags. The line is written into the file so the next start finds it.
    if site.is_some() && nfc_cfg.token.trim().is_empty() {
        match settings::ensure_token(&path) {
            Ok(Some(token)) => {
                println!("tea: wrote a token into {} for the settings page", path.display());
                nfc_cfg.token = token;
            }
            Ok(None) => {}
            Err(e) => eprintln!("tea: {e}"),
        }
    }
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
    // Said on every path, preview included: a switch that is on in the file and
    // silently off in the process is the worst kind of setting.
    if let Some(why) = nfc_cfg.steps_misconfigured() {
        eprintln!("tea: note: {why}");
    }
    if let Some(why) = nfc_cfg.moving_misconfigured() {
        eprintln!("tea: note: {why}");
    }
    if let Some(why) = nfc_cfg.chores_misconfigured() {
        eprintln!("tea: note: {why}");
    }
    if let Some(why) = nfc_cfg.home_assistant.publish_misconfigured() {
        eprintln!("tea: note: {why}");
    }
    if let Some(why) = look_cfg.accent_misconfigured() {
        eprintln!("tea: note: {why}");
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
        return run_probe(&nfc_cfg, &calls_cfg.ignore);
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
            if nfc_cfg.counts_steps() {
                println!(
                    "tea: and for {} real steps, counted from wherever you are standing now.",
                    nfc_cfg.steps.count
                );
            }
            if nfc_cfg.counts_moving() {
                println!(
                    "tea: and for {}s of your phone saying you are moving.",
                    nfc_cfg.moving_secs()
                );
            }
        }
        strict::restore_leftovers();
        return preview(total, sound_cfg, anim_cfg, hold_cfg, look_cfg, nfc_cfg);
    }

    if run_warning {
        let total = run_for.unwrap_or(cfg.warn_before);
        println!("tea: showing the warning for {}", human(total));
        return preview_warning(total, cfg.postpone, anim_cfg, look_cfg);
    }

    println!(
        "tea: work {}, break {}. Idle {}+ counts as a break.\n\
         config: {}\nCtrl-C to stop.",
        human(cfg.work),
        human(cfg.brk),
        human(cfg.idle_credit),
        path.display()
    );

    if hours_cfg.set() {
        println!("hours: awake {}", hours_cfg.describe());
    }
    if let Some(left) = state::off::left() {
        println!("off: switched off for another {} — `tea on` to start again", human(left));
    }
    if cfg.long_every > 0 {
        println!(
            "long: every {} break runs {} instead of {}",
            match cfg.long_every {
                2 => "2nd".to_string(),
                3 => "3rd".to_string(),
                n => format!("{n}th"),
            },
            human(cfg.long_brk),
            human(cfg.brk)
        );
    }

    if nfc_cfg.on() {
        println!(
            "nfc: on — the page waits for {}, {}",
            gate_words(&nfc_cfg),
            match cfg.release_grace {
                g if g.is_zero() => "for as long as it takes".to_string(),
                g => format!("giving up after {}", human(g)),
            }
        );
    }

    // Whatever mode this run is in: a strict break that was cut short by a
    // crash or a restart left the keyboard short of its Super key, and that
    // is put right before anything else happens.
    strict::restore_leftovers();

    if headless {
        run_headless(cfg, sound_cfg, nfc_cfg, hours_cfg, calls_cfg, site);
    } else {
        run_gtk(cfg, sound_cfg, anim_cfg, hold_cfg, look_cfg, nfc_cfg, hours_cfg, calls_cfg, site);
    }
}

/// What the page waits for, as a phrase: "the tag", "the tag and 100 steps",
/// "the tag, 100 steps and 30s on your feet".
fn gate_words(cfg: &nfc::Config) -> String {
    let mut parts = vec!["the tag".to_string()];
    if cfg.counts_steps() {
        parts.push(format!("{} steps", cfg.steps.count));
    }
    if cfg.counts_moving() {
        parts.push(format!("{}s on your feet", cfg.moving_secs()));
    }
    match parts.len() {
        1 => parts.remove(0),
        2 => parts.join(" and "),
        _ => {
            let last = parts.pop().unwrap_or_default();
            format!("{} and {last}", parts.join(", "))
        }
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
            "tea: note — port.listen is loopback, so only this machine can reach it.\n\
             \x20    A phone in another room needs either [port] listen = \"0.0.0.0:{port}\"\n\
             \x20    and a hole in the firewall, or nfc.url pointing at something\n\
             \x20    that is already listening. See \"The ear\" in the README."
        );
    }
}

/// Terminal-only: no GTK, no display needed. Works over SSH.
fn run_headless(
    cfg: tea_core::Config,
    sound: sound::Config,
    nfc: nfc::Config,
    hours: config::Hours,
    calls: config::Calls,
    site: Option<web::Site>,
) {
    let mut engine = Engine::start(cfg, sound, nfc, hours, calls, site);
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
#[allow(clippy::too_many_arguments)]
fn run_gtk(
    cfg: tea_core::Config,
    sound: sound::Config,
    anim: overlay::Anim,
    hold: overlay::Hold,
    look: overlay::Look,
    nfc: nfc::Config,
    hours: config::Hours,
    calls: config::Calls,
    site: Option<web::Site>,
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
        let engine =
            RefCell::new(Engine::start(
                cfg.clone(),
                sound.clone(),
                nfc.clone(),
                hours.clone(),
                calls.clone(),
                site.clone(),
            ));
        let ui = RefCell::new(GtkBlocker::new(
            app,
            engine.borrow().postpone_flag(),
            anim.clone(),
            hold,
            &look,
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
    look: overlay::Look,
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
            // The settings page stays with the service: a preview that
            // served it would be a second editor of the same file.
            match nfc::listen(&nfc, Rc::clone(&link), None) {
                Ok(e) => {
                    println!("nfc: listening on {}", e.addr);
                    ear = Some(e);
                }
                // Only worth a line when the ear was the only way a scan
                // could have arrived.
                Err(why) if !nfc.asks() => eprintln!("tea: {why}"),
                Err(_) => {}
            }
        }
        // The hub is asked whether or not the tag is part of this: the list in
        // the corner is read off the same poll, and it holds nothing back, so a
        // page with jobs on it and no gate at all is an ordinary way to run.
        if nfc.asks() && (asking || nfc.shows_chores()) {
            match nfc::watch(&nfc, Rc::clone(&link)) {
                Ok(w) => {
                    if asking {
                        println!(
                            "nfc: watching {} — scan the tag to lift the page",
                            nfc.home_assistant.entity.trim()
                        );
                        if nfc.counts_steps() {
                            println!(
                                "nfc: and counting {} steps of {} — the page wants both",
                                nfc.steps.count,
                                nfc.steps.entity.trim()
                            );
                        }
                    }
                    if nfc.shows_chores() {
                        println!("nfc: reading {}", nfc.chores.entity.trim());
                    }
                    watch = Some(w);
                }
                Err(why) => eprintln!("tea: {why}"),
            }
        }
        if asking && ear.is_none() && watch.is_none() {
            println!("nfc: nothing can hear a real scan, so the preview will act one out");
        }
        let wired = ear.is_some() || watch.is_some();

        let ui = Rc::new(RefCell::new(GtkBlocker::new(
            app,
            Rc::new(Cell::new(false)),
            anim.clone(),
            hold,
            &look,
            asking.then(|| nfc.prompt.clone()),
        )));
        let mut player = sound::Player::new(sound.clone());
        player.break_starts();
        // Before the page is built, so it is born with the walk on it: the
        // watch above has already said how far this break is asking you to go.
        if let Some(w) = link.walk() {
            ui.borrow_mut().steps_seen(w.walked, w.needed, w.marked);
        }
        // And the moving, for the same reason: a page born without the badge
        // never grows one.
        if let Some(m) = link.motion() {
            ui.borrow_mut().motion_seen(m.secs, m.needed, m.lost);
        }
        ui.borrow_mut().engage(total);

        let grace = nfc.grace.0;
        let left = Cell::new(total);
        let waited = Cell::new(Duration::ZERO);
        let shown = Cell::new(Duration::ZERO);
        let asked = Cell::new(false);
        // The tag half of the gate, and the whole of it: with steps counted
        // too, the second is not the first.
        let tag_in = Cell::new(false);
        let opened = Cell::new(false);
        let late = Cell::new(false);
        let cheat_since: Cell<Option<Duration>> = Cell::new(None);
        let busted = Cell::new(false);
        let app = app.clone();
        glib::timeout_add_seconds_local(1, move || {
            // Held here so the ear keeps listening and the watch keeps polling
            // for as long as the page is up.
            let _keep = (&ear, &watch);

            // A scan lands between ticks, exactly as it does in the daemon.
            // Early ones bank: the countdown still runs out.
            if link.take_scan() && !tag_in.replace(true) {
                println!("[scan]  the tag was scanned");
                ui.borrow_mut().tag_seen();
                player.scanned();
            }
            // And the walk, read the same way the daemon reads it.
            let walk = link.walk();
            if let Some(w) = walk {
                ui.borrow_mut().steps_seen(w.walked, w.needed, w.marked);
            }
            let motion = link.motion();
            if let Some(m) = motion {
                ui.borrow_mut().motion_seen(m.secs, m.needed, m.lost);
            }
            // And the list, so a preview shows the corner as a real break will
            // -- the day's count included, which is the list's to give and
            // not this page's: a preview is not a day, but it is on one.
            match link.board() {
                Some(board) => {
                    ui.borrow_mut().chores_seen(&board.title, &board.chores(), board.hidden, board.today)
                }
                None => ui.borrow_mut().chores_seen("", &[], 0, 0),
            }
            // The same verdict the daemon gives, so the teasing can be seen
            // without waiting out a work interval -- and without a tally,
            // because a preview is not a day.
            let shaken = walk.is_some_and(|w| w.walked >= CHEAT_STEPS)
                && motion.is_some_and(|m| m.secs == 0 && !m.lost);
            if shaken {
                let since = cheat_since.get().unwrap_or_else(|| {
                    let now = boottime();
                    cheat_since.set(Some(now));
                    now
                });
                if boottime().saturating_sub(since) >= CHEAT_AFTER && !busted.replace(true) {
                    println!(
                        "[nfc]   {} steps and not a second on your feet — nice try",
                        walk.map_or(0, |w| w.walked)
                    );
                }
            } else if motion.is_some_and(|m| m.secs > 0) {
                cheat_since.set(None);
                if busted.replace(false) {
                    println!("[nfc]   moving now — the walk counts again");
                }
            }
            ui.borrow_mut().cheat_seen(busted.get());
            if tag_in.get()
                && walk.is_none_or(|w| w.done())
                && motion.is_none_or(|m| m.done())
                && !opened.replace(true)
            {
                if let Some(w) = walk {
                    println!("[scan]  {} steps walked — that is the gate", w.walked);
                }
                late.set(asked.get());
                ui.borrow_mut().release_seen();
            }

            let remaining = left.get().saturating_sub(TICK);
            left.set(remaining);

            let done = if !remaining.is_zero() {
                ui.borrow_mut().update(remaining);
                false
            } else if !asking {
                true
            } else if opened.get() {
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
                    if waited.get() + THEN_LIFT_AFTER >= LOOK_AT_IT_FOR && !opened.replace(true) {
                        println!("[scan]  (preview) somebody pretends to scan the tag");
                        late.set(true);
                        tag_in.set(true);
                        ui.borrow_mut().tag_seen();
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
                waiting: asked.get() && !opened.get(),
                released: opened.get(),
                tag_in: tag_in.get(),
                steps_left: match opened.get() {
                    true => 0,
                    false => walk.map_or(0, |w| w.left()),
                },
            });
            glib::ControlFlow::Continue
        });
    });
    let argv: Vec<String> = std::env::args().take(1).collect();
    app.run_with_args(&argv);
}

/// Show the warning toast on its own, so its wording and behaviour can be
/// checked without sitting through a work interval.
fn preview_warning(total: Duration, postpone: Duration, anim: overlay::Anim, look: overlay::Look) {
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
            &look,
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
    /// The other direction: what tea tells the hub. `None` when it tells it
    /// nothing, which is the default.
    publish: Option<publish::Publisher>,
    /// The warning is up and the break has not started. Only the hub cares:
    /// the page and the scheduler each know this already, in their own way.
    warned: bool,
    /// The moving half has been announced as done for the break on screen.
    moved_told: bool,
    /// When steps started arriving with no movement to show for them, on the
    /// boot clock. `None` until they do; cleared the moment the phone moves.
    cheat_since: Option<Duration>,
    /// The verdict, once given, for the break on screen.
    busted: bool,
    /// The tag has been scanned for the break on screen.
    ///
    /// Held here rather than handed straight to the scheduler, because with
    /// steps counted too a scan is only half of what ends a break. Not
    /// persisted: a service restarted mid-break loses the step count as well --
    /// the poll starts a fresh baseline -- and keeping half a gate across a
    /// restart would let the other half be walked twice.
    tag_in: bool,
    /// When tea is awake at all, and why it is not.
    ///
    /// Two switches, one behaviour: `[hours]` is the standing one and
    /// `tea off 1h` is the afternoon one. The reason is held as the sentence
    /// that will be printed, so waking and sleeping are announced once rather
    /// than every second of a Sunday.
    hours: config::Hours,
    asleep: Option<String>,
    /// What today came to. Read back out of the state file at startup, so a
    /// restart at four o'clock does not lose the morning.
    tally: state::Tally,
}

impl Engine {
    fn start(
        cfg: tea_core::Config,
        sound: sound::Config,
        nfc_cfg: nfc::Config,
        hours: config::Hours,
        calls: config::Calls,
        site: Option<web::Site>,
    ) -> Self {
        let now = boottime();
        let store = state::Store::new();
        let restored = store.as_ref().and_then(|s| s.load(now));

        let mut tally = state::Tally::default();
        let (sched, catchup) = match restored {
            Some(r) => {
                tally = r.tally.clone();
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
        // The same port serves the settings page, so it opens for either.
        let serving = site.is_some();
        let ear = (nfc_cfg.on() || serving)
            .then(|| nfc::listen(&nfc_cfg, Rc::clone(&link), site))
            .and_then(|r| match r {
                Ok(ear) => {
                    println!("nfc: listening on {}", ear.addr);
                    if serving {
                        println!("settings: page at {}", web::url(&nfc_cfg));
                    }
                    Some(ear)
                }
                Err(e) => {
                    eprintln!("tea: the port is closed — {e}");
                    None
                }
            });

        // Asking is the tidier half of this: nothing has to be forwarded in, and
        // an unreachable hub is a thing tea finds out about by itself.
        let watch = nfc_cfg.asks().then(|| nfc::watch(&nfc_cfg, Rc::clone(&link)))
            .and_then(|r| match r {
                Ok(watch) => {
                    println!(
                        "nfc: watching {} on {}",
                        nfc_cfg.home_assistant.entity, nfc_cfg.home_assistant.url
                    );
                    if nfc_cfg.counts_steps() {
                        println!(
                            "nfc: and {} steps of {}, walked while the page is up",
                            nfc_cfg.steps.count,
                            nfc_cfg.steps.entity.trim()
                        );
                    }
                    if nfc_cfg.counts_moving() {
                        println!(
                            "nfc: and {}s of {} saying you are moving",
                            nfc_cfg.moving_secs(),
                            nfc_cfg.moving.entity.trim()
                        );
                    }
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

        // Reporting is the tidy half of the tidy half: nothing is asked of the
        // hub at all, it is only told. A hub that cannot be told is a line on
        // stderr and nothing else -- see `publish`.
        let publish = nfc_cfg
            .home_assistant
            .publishes()
            .then(|| publish::Publisher::new(&nfc_cfg.home_assistant))
            .and_then(|r| match r {
                Ok(publisher) => {
                    println!(
                        "ha: reporting to {} as {}, with {} numbers beside it",
                        nfc_cfg.home_assistant.url.trim(),
                        publisher.entity(),
                        publisher.siblings()
                    );
                    Some(publisher)
                }
                Err(e) => {
                    eprintln!("tea: not reporting to Home Assistant — {e}");
                    None
                }
            });

        Self {
            sched,
            session: Session::connect().ignoring(calls.ignore),
            store,
            last: now,
            catchup,
            postpone: Rc::new(Cell::new(false)),
            sound: sound::Player::new(sound),
            link,
            _ear: ear,
            _watch: watch,
            publish,
            warned: false,
            moved_told: false,
            cheat_since: None,
            busted: false,
            tag_in: false,
            hours,
            asleep: None,
            tally,
        }
    }

    /// Why tea should be doing nothing at all right now, said the way it will
    /// be printed. `None` means get on with it.
    ///
    /// Both answers freeze everything rather than merely hiding the page: an
    /// evening film is not a work session with the timer paused, and coming
    /// back on Monday to a break that fell due on Saturday is exactly the
    /// ambush the idle rules exist to prevent.
    fn dormant(&self) -> Option<String> {
        if let Some(left) = state::off::left() {
            return Some(format!("off for another {}", human(left)));
        }
        if !self.hours.awake(clock::now()) {
            return Some(format!("outside working hours — {}", self.hours.opens()));
        }
        None
    }

    /// One more break in the bag, and however far it was walked.
    ///
    /// The walk is read from the value the page was last shown rather than
    /// asked for again: by the time a break ends the poll has already been
    /// told to forget it, and a day's walking should not depend on which of
    /// the two happened first.
    ///
    /// `ran` is the length this particular break had -- taken from the snapshot
    /// before the tick that ended it, because by now the scheduler has moved on
    /// and `break_length()` is already answering about the *next* one.
    fn count_break(
        &mut self,
        walk: Option<nfc::Walk>,
        motion: Option<nfc::Motion>,
        chores: u32,
        ran: Duration,
        gate: history::Gate,
    ) {
        self.tally.breaks += 1;
        self.tally.steps += walk.map_or(0, |w| w.walked);
        self.tally.chores += chores;
        history::append(&history::Event::Break {
            t: clock::unix_now(),
            len: ran.as_secs(),
            steps: walk.map_or(0, |w| w.walked),
            moved: motion.map_or(0, |m| m.secs),
            cheated: self.busted,
            needed: walk.map_or(0, |w| w.needed),
            chores,
            gate,
            long: !ran.is_zero() && ran != self.sched.config().brk,
        });
        self.tell(
            "break_end",
            json!({
                "len": ran.as_secs(),
                "steps": walk.map_or(0, |w| w.walked),
                "moved": motion.map_or(0, |m| m.secs),
                "cheated": self.busted,
                "chores": chores,
                "gate": gate,
                "long": !ran.is_zero() && ran != self.sched.config().brk,
            }),
        );
    }

    /// How the break that just ended actually ended, which is the only thing
    /// recorded here that says whether the tag is earning its place.
    ///
    /// `before` is the state on the way into the tick, so its `released` is the
    /// scan as it stood while the page was still up.
    fn gate_of(
        &self,
        before: &tea_core::Snapshot,
        walk: Option<nfc::Walk>,
        motion: Option<nfc::Motion>,
        gave_up: bool,
    ) -> history::Gate {
        history::gate(
            self.sched.config().require_release,
            gave_up,
            before.released,
            walk.is_some_and(|w| w.needed > 0) || motion.is_some_and(|m| m.needed > 0),
        )
    }

    fn postpone_flag(&self) -> Rc<Cell<bool>> {
        Rc::clone(&self.postpone)
    }

    /// Something happened, said to the hub -- if there is a hub to say it to.
    fn tell(&self, what: &str, data: serde_json::Value) {
        if let Some(publisher) = &self.publish {
            publisher.event(what, data);
        }
    }

    /// And how things stand now. Called every tick; the publisher drops
    /// anything the hub has already been told.
    ///
    /// `why_off` is the sentence tea printed when it went to sleep, and its
    /// presence is what makes the state `off`: everything else is read from
    /// the snapshot the tick just produced.
    fn report(&self, after: &tea_core::Snapshot, why_off: Option<String>) {
        let Some(publisher) = &self.publish else {
            return;
        };
        let now = clock::unix_now();
        let cfg = self.sched.config();
        let waiting = self.sched.awaiting_release();
        let walk = self.link.walk();
        let state = match (why_off.is_some(), after.breaking, waiting, after.due, self.warned) {
            (true, ..) => publish::State::Off,
            (_, true, true, ..) => publish::State::Waiting,
            (_, true, ..) => publish::State::Break,
            (_, _, _, true, _) => publish::State::Held,
            (_, _, _, _, true) => publish::State::Warning,
            _ => publish::State::Working,
        };
        let awake = why_off.is_none();
        publisher.report(publish::Report {
            state,
            next_break_at: (awake && !after.breaking)
                .then(|| now + cfg.work.saturating_sub(after.worked).as_secs()),
            break_ends_at: (after.breaking && !waiting)
                .then(|| now + after.break_len.saturating_sub(after.rested).as_secs()),
            break_len: after.break_len.as_secs(),
            long: after.breaking && after.break_len != cfg.brk,
            worked_min: after.worked.as_secs() / 60,
            tag_scanned: self.tag_in,
            steps_walked: walk.map_or(0, |w| w.walked),
            steps_needed: walk.map_or(0, |w| w.needed),
            moving_secs: self.link.motion().map_or(0, |m| m.secs),
            moving_needed: self.link.motion().map_or(0, |m| m.needed),
            postpones_left: self.sched.postpones_left(),
            breaks_today: self.tally.breaks,
            steps_today: self.tally.steps,
            chores_today: self.tally.chores,
            credited_today: self.tally.credited,
            postponed_today: self.tally.postponed,
            cheats_today: self.tally.cheats,
            worked_today_min: self.tally.worked_ms / 60_000,
            why_off,
        });
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
        // Before anything else: whether tea is supposed to be awake. Off, or
        // out of hours, means the clock does not run -- time spent here is
        // neither work banked nor rest credited, it simply did not happen.
        match self.dormant() {
            Some(why) => {
                if self.asleep.is_none() {
                    println!("[off]   {why}");
                    self.tell("off", json!({ "why": why }));
                    // An afternoon off that begins in the middle of a break
                    // takes the page with it. "Leave me alone" starting with
                    // five minutes of not being left alone is a joke.
                    if self.sched.snapshot().breaking {
                        self.sched.skip_break();
                        ui.release();
                    }
                    ui.clear_warning();
                }
                self.asleep = Some(why);
                // The downtime is dropped rather than banked: coming back from
                // an hour off to a break that fell due during it would be the
                // whole feature undone on the second tick.
                self.catchup = None;
                let now = boottime();
                self.last = now;
                // No break on screen and no scan outstanding, said out loud:
                // the Home Assistant poll runs only while a page is up, and a
                // desk left saying "breaking" would have it asking the hub
                // about a tag once a second for the whole of a Sunday.
                self.tag_in = false;
                self.link.post(nfc::Desk::default());
                // Still written, though nothing moved. The state file is how
                // `tea status` knows the service is alive at all, and a tea
                // that is deliberately quiet must not read as a tea that has
                // fallen over.
                if let Some(store) = &mut self.store {
                    store.save(self.sched.snapshot(), &self.tally, now, false);
                }
                // And the hub, which would otherwise be left showing a clock
                // that is not running.
                let why = self.asleep.clone();
                self.report(&self.sched.snapshot(), why);
                return;
            }
            None => {
                if self.asleep.take().is_some() {
                    println!("[on]    back on — the clock is running again");
                    self.tell("on", json!({}));
                    self.catchup = None;
                    self.last = boottime();
                }
            }
        }

        // Yesterday's four breaks are not today's.
        self.tally.roll(&clock::today());

        // The server never touches the scheduler; this is where a scan that
        // arrived between ticks actually lands.
        let mut celebrated = false;
        // Whether the scan landed on this very tick. With steps counted too the
        // scan and the gate opening are separate moments, sometimes minutes
        // apart, and each wants its own sound; without them they are the same
        // moment, which must not be chimed at twice.
        let mut scanned_now = false;
        let desk = self.sched.snapshot();
        let walk = self.link.walk();
        let motion = self.link.motion();
        // Read here rather than at the end of the break, like the walk and for
        // the same reason: by the time a break is counted the poll has already
        // been told to forget it, and a day's jobs should not depend on which
        // of the two happened first.
        let chores = self.link.chores_done();
        if self.link.take_scan() && !self.tag_in {
            if desk.breaking {
                // Banked only against a break that already exists. A tag
                // touched on the way past while still working must not sit here
                // waiting to half-open the break that starts on this very tick.
                self.tag_in = true;
                // The scan is acknowledged the moment it lands, whether or not
                // it is the thing that ends the break: a walk to the tag that
                // changes nothing on screen is a walk somebody makes twice.
                ui.tag_seen();
                self.sound.scanned();
                scanned_now = true;
                self.tell("scan", json!({}));
                match walk {
                    Some(w) if !w.done() => {
                        println!("[nfc]   tag scanned — {} more steps to walk", w.left());
                    }
                    _ if motion.is_some_and(|m| !m.done()) => println!(
                        "[nfc]   tag scanned — {}s more on your feet",
                        motion.map_or(0, |m| m.left())
                    ),
                    _ => println!("[nfc]   tag scanned"),
                }
            } else {
                println!("[nfc]   tag scanned — no break to end");
            }
        }

        // Painted before the gate is tested, not after: the step that opens it
        // is the one worth seeing land, and a page told about it only on the
        // next tick would celebrate while the badge still said nineteen. And
        // between breaks as well as during them, because this is also how the
        // page learns there is a walk in this break at all -- a page built by
        // the tick below has to be born with the badge on it.
        if let Some(w) = self.link.walk() {
            ui.steps_seen(w.walked, w.needed, w.marked);
        }
        // The list in the corner, told every tick for the same reason the
        // badges are: a page rebuilt by `insist` has to come back carrying the
        // same jobs, and the blocker does the work only when something moved.
        // Today's count comes with the list, off the hub's own stamps, rather
        // than from the tally: the tally only knows what was done while one
        // of *these* pages was up, and a job ticked off over lunch, or on a
        // `tea run` page, is still a job done today.
        match self.link.board() {
            Some(board) => ui.chores_seen(&board.title, &board.chores(), board.hidden, board.today),
            None => ui.chores_seen("", &[], 0, 0),
        }
        if let Some(m) = motion {
            ui.motion_seen(m.secs, m.needed, m.lost);
            if m.done() && desk.breaking && !self.moved_told {
                self.moved_told = true;
                println!("[ha]    the moving is in");
                self.tell("moved", json!({ "secs": m.secs }));
            }
        }

        // A hand is not a walk. The two sensors disagree in exactly one way
        // that only a shaken phone produces -- steps climbing while the
        // activity sensor keeps saying still -- and once enough of them have
        // done so for long enough, the page says so. The gate is not touched:
        // it is still waiting for the walk, which is the whole of the point.
        let shaken = desk.breaking
            && walk.is_some_and(|w| w.walked >= CHEAT_STEPS)
            && motion.is_some_and(|m| m.secs == 0 && !m.lost);
        if shaken {
            let since = *self.cheat_since.get_or_insert_with(boottime);
            if boottime().saturating_sub(since) >= CHEAT_AFTER && !self.busted {
                self.busted = true;
                self.tally.cheats += 1;
                println!(
                    "[nfc]   {} steps and not a second on your feet — nice try",
                    walk.map_or(0, |w| w.walked)
                );
                self.tell("cheat", json!({ "steps": walk.map_or(0, |w| w.walked) }));
            }
        } else if motion.is_some_and(|m| m.secs > 0) {
            self.cheat_since = None;
            if std::mem::take(&mut self.busted) {
                println!("[nfc]   moving now — the walk counts again");
            }
        }
        ui.cheat_seen(self.busted);

        // Both halves, or neither. The tag says you got up; the steps say you
        // went somewhere -- and a tag within reach of the chair is exactly the
        // hole this closes.
        let walked = walk.is_none_or(|w| w.done());
        let moved = motion.is_none_or(|m| m.done());
        if self.tag_in && walked && moved && !desk.released {
            let opened = self.sched.released();
            let landed = matches!(opened, ReleaseResult::Freed | ReleaseResult::Banked { .. });
            if landed {
                self.tell("released", json!({ "steps": walk.map_or(0, |w| w.walked) }));
            }
            // The gate opening is the moment worth hearing, and with steps
            // counted it is not the moment the tag was scanned -- that may have
            // been minutes ago, and had its own chime then. Only when the two
            // fall on the same tick has this already been played.
            if landed && !scanned_now {
                self.sound.scanned();
            }
            match opened {
                ReleaseResult::Freed => {
                    println!("[nfc]   the walk is in — the desk is yours");
                    // Told now, not via the next snapshot: the break resets on
                    // this very tick, and a page torn down without ever
                    // acknowledging the walk reads as a scan that was
                    // swallowed. This is also what starts the celebration.
                    ui.release_seen();
                    celebrated = true;
                }
                ReleaseResult::Banked { remaining } => {
                    println!(
                        "[nfc]   the walk is in — {} of the break still to run",
                        human(remaining)
                    );
                    ui.release_seen();
                }
                ReleaseResult::NotBreaking => {}
                ReleaseResult::NotRequired => {
                    // Nothing can reach this while the ear and the watch both
                    // need the gate switched on -- but a scan that changes
                    // nothing must not say so once a second if one ever does.
                    self.tag_in = false;
                    println!("[nfc]   tag scanned — nothing was waiting");
                }
            }
        }

        if self.postpone.replace(false) {
            match self.sched.postpone() {
                PostponeResult::Granted { remaining_budget } => {
                    println!("[snooze] postponed, {remaining_budget} left this window");
                    self.tally.postponed += 1;
                    history::append(&history::Event::Postpone { t: clock::unix_now() });
                    ui.clear_warning();
                    self.warned = false;
                    self.tell("postpone", json!({ "left": remaining_budget }));
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
        // Work banked this tick goes on the day's total. Only rises count: the
        // fall to zero at a break is the same time counted once already.
        if after.worked > before.worked {
            self.tally.worked_ms += (after.worked - before.worked).as_millis() as u64;
        }
        // Asked of the whole batch rather than tracked as the loop goes, so it
        // does not matter which order the two commands come out in.
        let gave_up =
            commands.iter().any(|c| matches!(c, tea_core::Command::GaveUpWaiting { .. }));
        let gate = self.gate_of(&before, walk, motion, gave_up);

        for cmd in &commands {
            match *cmd {
                tea_core::Command::Overdue { waiting } => {
                    self.report_overdue(waiting);
                    self.tell("held", json!({ "waiting": waiting.as_secs() }));
                }
                tea_core::Command::AwaitRelease => self.tell("waiting", json!({})),
                tea_core::Command::Warn { until_break } => {
                    self.warned = true;
                    self.tell("warning", json!({ "in": until_break.as_secs() }));
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
                tea_core::Command::ShowOverlay { duration } => {
                    self.sound.break_starts();
                    self.warned = false;
                    self.tell(
                        "break_start",
                        json!({
                            "len": duration.as_secs(),
                            "long": duration != self.sched.config().brk,
                        }),
                    );
                }
                // Counted where it ends rather than where it starts: a break
                // that was interrupted by a shutdown was not a break you took.
                tea_core::Command::CreditedIdle { was_idle } => {
                    self.tally.credited += 1;
                    self.warned = false;
                    self.tell("credited", json!({ "idle": was_idle.as_secs() }));
                    history::append(&history::Event::Credited {
                        t: clock::unix_now(),
                        idle: was_idle.as_secs(),
                    });
                }
                // The scan that just freed this break has its own sound; the
                // ordinary end-of-break one stacked on top would turn the
                // celebration into a clatter.
                tea_core::Command::HideOverlay if !celebrated => {
                    self.count_break(walk, motion, chores, before.break_len, gate);
                    self.sound.break_ends();
                }
                tea_core::Command::HideOverlay => {
                    self.count_break(walk, motion, chores, before.break_len, gate)
                }
                _ => {}
            }
        }

        // Told every tick, not just the one the scan landed on: a page that is
        // rebuilt -- or resumed after a restart -- has to come back knowing the
        // walk was already made. The blocker does the work once.
        if after.breaking && after.released {
            ui.release_seen();
        }

        // A break that has ended takes its half-open gate with it, or the next
        // one would start already scanned for.
        if !after.breaking {
            self.tag_in = false;
            self.moved_told = false;
            self.cheat_since = None;
            self.busted = false;
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
            // The break on screen, not the ordinary one: every so often it is
            // the long one, and measuring against `cfg.brk` had the phone told
            // "the page lifts on its own in 0s" for the last ten minutes of a
            // fifteen-minute break. Zero between breaks, where nothing reads it.
            remaining: after.break_len.saturating_sub(after.rested),
            waiting: self.sched.awaiting_release(),
            released: after.released,
            tag_in: self.tag_in,
            steps_left: match after.breaking && !after.released {
                true => self.link.walk().map_or(0, |w| w.left()),
                false => 0,
            },
        });

        // Every tick, and cheap to: the publisher only sends what has changed.
        self.report(&after, None);

        if let Some(store) = &mut self.store {
            // Anything that would hurt to lose gets written immediately; plain
            // accumulation rides the throttle.
            let notable = before.breaking != after.breaking
                || before.postpones_used != after.postpones_used
                || after.worked < before.worked;
            store.save(after, &self.tally, now, notable);
        }
    }
}

/// Print what the session reports, once a second, so idle detection can be
/// eyeballed without waiting out a work interval.
fn run_probe(nfc_cfg: &nfc::Config, ignore: &[String]) {
    if nfc_cfg.asks() {
        let ha = &nfc_cfg.home_assistant;
        // Which slot the name came from, not what it says: a steps sensor that
        // happens to be spelled the same as the tag must not decide whether the
        // tag gets probed at all.
        let asked = [
            (ha.entity.trim(), true),
            (nfc_cfg.steps.entity.trim(), nfc_cfg.counts_steps()),
            (nfc_cfg.moving.entity.trim(), nfc_cfg.counts_moving()),
        ];
        for (entity, wanted) in asked {
            // The steps sensor is the one that is easiest to get wrong and the
            // hardest to notice: a mistyped tag is a break that will not lift,
            // a mistyped sensor is the same thing with no obvious culprit.
            if !wanted || entity.is_empty() {
                continue;
            }
            print!("tea: asking {} about {} ... ", ha.url, entity);
            match nfc::probe(ha, entity) {
                Ok(state) => println!("{state:?}"),
                Err(e) => println!("\ntea: {e}"),
            }
        }
        println!();
    }

    // The other direction gets the same treatment: a token that can read but
    // not write is caught at the desk, not noticed as a hall light that never
    // came on.
    if nfc_cfg.home_assistant.publishes() {
        let ha = &nfc_cfg.home_assistant;
        print!("tea: telling {} ... ", ha.url.trim());
        match publish::probe(ha) {
            Ok(said) => println!("{said}"),
            Err(e) => println!("\ntea: {e}"),
        }
        println!();
    } else if let Some(why) = nfc_cfg.home_assistant.publish_misconfigured() {
        println!("tea: not telling Home Assistant anything — {why}\n");
    }

    let mut session = Session::connect().ignoring(ignore.to_vec());
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
        ("dash", "steps, breaks and how the habit is going"),
        ("settings", "open the settings page in your browser"),
        ("reload", "pick up changed settings"),
        ("off [dur]", "no breaks for a while (an hour by default)"),
        ("on", "start again, now"),
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
        ("    --no-open", "with `dash`: write the page, don't open it"),
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

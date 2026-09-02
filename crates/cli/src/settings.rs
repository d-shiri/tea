//! `tea config` and `tea set-*` — reading and changing settings without
//! opening an editor.

use crate::config::{self, FileConfig, human, literal};
use crate::nfc;
use crate::status::{Style, WIDTH, heading, tilde};
use tea_core::Config;
use std::path::Path;

/// Change one duration in the config file, keeping every comment and blank line
/// exactly where it was — which is why this uses `toml_edit` rather than
/// re-serialising the parsed config.
pub fn set(path: &Path, key: &str, raw: &str) -> Result<String, String> {
    let value = config::parse(raw)
        .ok_or_else(|| format!("{raw:?} is not a duration like 25m, 90s or 1h"))?;

    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| format!("{} is not valid TOML:\n{e}", path.display()))?;

    let was = doc
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| "unset".to_string());
    // Assigning a fresh item would throw away the line's decor -- including
    // the trailing comment explaining what the setting does. Replace the value
    // in place and put the decor back.
    match doc.get_mut(key).and_then(|item| item.as_value_mut()) {
        Some(existing) => {
            let decor = existing.decor().clone();
            *existing = toml_edit::Value::from(literal(value));
            *existing.decor_mut() = decor;
        }
        None => doc[key] = toml_edit::value(literal(value)),
    }
    let updated = doc.to_string();

    // Never write something that will not start. Parsing the result back is the
    // same path the daemon takes, so if it survives this it will survive boot.
    let parsed: FileConfig = toml::from_str(&updated)
        .map_err(|e| format!("that change would break the config:\n{e}"))?;
    let mut cfg: Config = parsed.into();
    config::reconcile(&mut cfg)?;

    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &updated).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))?;

    Ok(format!("{key}: {was} → {}", literal(value)))
}

/// Point the chime at a sound file and switch sound on, in one step.
pub fn set_sound(path: &Path, file: &Path) -> Result<String, String> {
    if !file.is_file() {
        return Err(format!("no such file: {}", file.display()));
    }
    let file = file
        .canonicalize()
        .map_err(|e| format!("cannot resolve {}: {e}", file.display()))?;

    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| format!("{} is not valid TOML:\n{e}", path.display()))?;

    if !doc.contains_key("sound") {
        doc["sound"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    doc["sound"]["start_file"] = toml_edit::value(file.to_string_lossy().into_owned());
    // Naming a file and leaving sound switched off would be a trap.
    doc["sound"]["mode"] = toml_edit::value("chime");

    let updated = doc.to_string();
    let parsed: FileConfig = toml::from_str(&updated)
        .map_err(|e| format!("that change would break the config:\n{e}"))?;
    let mut cfg: Config = parsed.into();
    config::reconcile(&mut cfg)?;

    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &updated).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))?;

    Ok(format!("sound: chime, {}", file.display()))
}

/// Turn the tag on or off, writing a token the first time it is needed.
///
/// Same one-step spirit as `set_sound`: switching it on and leaving no way to
/// authenticate would be a trap, so the secret is made here rather than left as
/// homework.
pub fn set_nfc(path: &Path, switch: &str) -> Result<String, String> {
    let on = match switch.trim().to_lowercase().as_str() {
        "on" | "enabled" | "active" | "yes" | "true" => true,
        "off" | "disabled" | "inactive" | "no" | "false" => false,
        other => return Err(format!("{other:?}: say `tea set-nfc on`, or `tea set-nfc off`")),
    };

    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| format!("{} is not valid TOML:\n{e}", path.display()))?;

    if !doc.contains_key("nfc") {
        doc["nfc"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let was = doc["nfc"].get("mode").and_then(|v| v.as_str()).unwrap_or("off").to_string();
    put(&mut doc["nfc"], "mode", if on { "on" } else { "off" });

    // Only ever adds one. Rewriting the token on every `set-nfc on` would
    // silently break a tag that is already on the wall.
    let known = doc["nfc"]
        .get("token")
        .and_then(|v| v.as_str())
        .is_some_and(|t| !t.trim().is_empty());
    let minted = on && !known;
    if minted {
        put(&mut doc["nfc"], "token", &crate::nfc::fresh_token()?);
    }

    let updated = doc.to_string();
    let parsed: FileConfig = toml::from_str(&updated)
        .map_err(|e| format!("that change would break the config:\n{e}"))?;
    let mut cfg: Config = parsed.into();
    config::reconcile(&mut cfg)?;

    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &updated).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))?;
    if known || minted {
        // The file now holds something worth keeping to yourself.
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }

    let mode = if on { "on" } else { "off" };
    Ok(match (was == mode, minted) {
        (_, true) => format!("nfc: {was} → {mode}, with a fresh token"),
        (true, _) => format!("nfc: already {mode}"),
        _ => format!("nfc: {was} → {mode}"),
    })
}

/// Replace a value without losing the comment sitting beside it, adding the key
/// if it was never there.
fn put(table: &mut toml_edit::Item, key: &str, value: &str) {
    match table.get_mut(key).and_then(|item| item.as_value_mut()) {
        Some(existing) => {
            let decor = existing.decor().clone();
            *existing = toml_edit::Value::from(value);
            *existing.decor_mut() = decor;
        }
        None => table[key] = toml_edit::value(value),
    }
}

pub fn show(cfg: &Config, file: &FileConfig, path: &Path) {
    let s = Style::new();

    println!();
    heading(&s, "tea settings", &tilde(path));

    section(&s, "timing");
    field(&s, "work", &human(cfg.work), "before a break falls due");
    field(&s, "break", &human(cfg.brk), "how long you rest");
    field(&s, "warning", &human(cfg.warn_before), "heads-up before it appears");
    match cfg.long_every {
        0 => field(&s, "long break", "off", "every break is the same length"),
        every => field(
            &s,
            "long break",
            &human(cfg.long_brk),
            &format!("every {every} breaks, instead of {}", human(cfg.brk)),
        ),
    }

    section(&s, "when tea is awake");
    let hours = &file.hours;
    if hours.set() {
        field(&s, "hours", &hours.describe(), "outside these, nothing is counted");
    } else {
        field(&s, "hours", "always", "no time of day is off limits");
    }
    match crate::state::off::left() {
        Some(left) => field(&s, "right now", "off", &format!("for another {} — `tea on`", human(left))),
        None => field(&s, "right now", "on", "`tea off 1h` stops it for a while"),
    }

    section(&s, "away from the keyboard");
    field(&s, "counts as a break", &human(cfg.idle_credit), "and the break is taken");
    field(&s, "pauses the timer", &human(cfg.idle_pause), "and the clock stops");

    section(&s, "postpone");
    if cfg.postpone_budget == 0 {
        field(&s, "allowance", "off", "no postponing");
    } else {
        field(&s, "each press", &human(cfg.postpone), "how much time it buys");
        field(
            &s,
            "allowance",
            &format!("{}/{}", cfg.postpone_budget, human(cfg.postpone_window)),
            "presses per window",
        );
    }

    section(&s, "calls");
    if cfg.defer_warn_after.is_zero() {
        field(&s, "warn after", "off", "never mention a held-up break");
    } else {
        field(&s, "warn after", &human(cfg.defer_warn_after), "if a call holds a break up");
    }

    section(&s, "sound");
    let sound = &file.sound;
    let mode = format!("{:?}", sound.mode).to_lowercase();
    field(&s, "mode", &mode, match sound.mode {
        crate::sound::Mode::Off => "breaks are silent",
        crate::sound::Mode::Chime => "plays a file",
        crate::sound::Mode::Voice => "speaks",
        crate::sound::Mode::Both => "plays a file and speaks",
    });
    // File names and spoken lines are far longer than any other value here, so
    // they go in the notes column. Squeezing them into the value column drags
    // every note on the page out of line with the rest.
    if matches!(sound.mode, crate::sound::Mode::Chime | crate::sound::Mode::Both) {
        field(&s, "on break start", "", &named(&sound.start_file));
        field(&s, "on break end", "", &named(&sound.end_file));
        let scan = match sound.scan_file.as_os_str().is_empty() {
            true => "the built-in chime".to_string(),
            false => named(&sound.scan_file),
        };
        field(&s, "on scan", "", &scan);
    }
    if matches!(sound.mode, crate::sound::Mode::Voice | crate::sound::Mode::Both) {
        field(&s, "says", "", &format!("\"{}\"", sound.start_words));
        field(&s, "then", "", &format!("\"{}\"", sound.end_words));
    }

    section(&s, "animation");
    let anim = &file.animation;
    if anim.entrance.0.is_zero() {
        field(&s, "entrance", "off", "the page just appears");
    } else {
        field(&s, "entrance", &human(anim.entrance.0), "how long the page takes to arrive");
        field(&s, "burst", &format!("{:.2}", anim.burst), "share of that spent exploding");
        field(&s, "shards", &anim.shards.to_string(), "pieces of debris");
    }

    section(&s, "holding the screen");
    match file.hold.mode {
        crate::overlay::Grip::Soft => {
            field(&s, "switching away", "soft", "the page stays put, behind what you switched to")
        }
        crate::overlay::Grip::Insist => {
            field(&s, "switching away", "insist", "the page puts itself back in front");
            field(&s, "rechecks every", &literal(file.hold.recheck.0), "how often it looks");
        }
    }

    section(&s, "the page");
    let look = &file.page;
    match look.accent_misconfigured() {
        Some(why) => field(&s, "accent", "default", &why),
        None => field(&s, "accent", look.accent.trim(), "the ring, the glow, the pills"),
    }
    match look.background {
        crate::overlay::Background::Dark => field(&s, "background", "dark", "covers the screen"),
        crate::overlay::Background::Dim => {
            field(&s, "background", "dim", "the desk shows through, darkened")
        }
    }
    match look.font.trim() {
        "" => field(&s, "font", "", "the first monospaced face the machine has"),
        face => field(&s, "font", "", face),
    }
    if look.prompts.on() {
        field(&s, "prompts", "", &look.prompts.describe());
        field(&s, "each stays up", &human(look.prompt_every.0), "then the next one");
    } else {
        field(&s, "prompts", "off", "the page says its one line and no more");
    }

    section(&s, "the tag");
    let nfc = &file.nfc;
    // Said whether or not the tag is on, because the list is not part of the
    // gate: a page can perfectly well show jobs and end on its own countdown.
    let jobs = || {
        if nfc.shows_chores() {
            field(
                &s,
                "jobs",
                &nfc.chores.cap().to_string(),
                &format!("from {}, shown in the corner of the page", nfc.chores.entity.trim()),
            );
        } else if let Some(why) = nfc.chores_misconfigured() {
            field(&s, "jobs", "off", &why);
        }
    };
    if !nfc.on() {
        field(&s, "nfc", "off", "breaks end when the countdown does");
        jobs();
    } else {
        field(&s, "nfc", "on", "the page waits to be released by a scan");
        if nfc.asks() {
            let ha = &nfc.home_assistant;
            field(&s, "watches", "", &format!("{} on {}", ha.entity, ha.url));
            field(&s, "asks every", &literal(ha.poll.0), "while a break is on screen");
        }
        if nfc.counts_steps() {
            field(&s, "steps", &nfc.steps.count.to_string(), "walked before the page lifts");
            let sync = match nfc.steps.sync {
                nfc::Sync::Live => "live — every rise counts",
                nfc::Sync::Batched => "batched — the first rise only moves the mark",
            };
            field(&s, "counted from", "", &format!("{}, {sync}", nfc.steps.entity));
        } else if let Some(why) = nfc.steps_misconfigured() {
            field(&s, "steps", "off", &why);
        } else {
            field(&s, "steps", "off", "the scan is the whole gate");
        }
        if nfc.counts_moving() {
            field(&s, "moving", "", &format!("{} on your feet before the page lifts", nfc.moving_words()));
            field(
                &s,
                "read from",
                "",
                &format!("{} — {}", nfc.moving.entity.trim(), nfc.moving.states.join(", ")),
            );
        } else if let Some(why) = nfc.moving_misconfigured() {
            field(&s, "moving", "off", &why);
        }
        if nfc.asks() {
            field(&s, "answers on", "", &format!("{} — for `tea unlock` only", nfc.listen));
        } else {
            field(&s, "answers on", "", &nfc.listen);
        }
        if nfc.fronted() {
            field(&s, "reached through", "", &nfc.url);
        }
        if nfc.grace.0.is_zero() {
            field(&s, "gives up after", "never", "the page waits for as long as it takes");
        } else {
            field(&s, "gives up after", &human(nfc.grace.0), "then hands the desk back anyway");
        }
        jobs();
        field(&s, "page says", "", &format!("\"{}\"", nfc.prompt));
        // Never the token itself: this output gets pasted into terminals that
        // other people read over.
        if nfc.asks() {
            field(&s, "hub token", "", &match nfc.home_assistant.secret() {
                // The source, never the secret: this output gets read over
                // shoulders and pasted into issues.
                Ok(_) => format!("from {}", tilde(std::path::Path::new(&nfc.home_assistant.secret_source()))),
                Err(why) => format!("MISSING — {why}"),
            });
        } else {
            field(&s, "token", "", match nfc.token.trim().is_empty() {
                true => "MISSING — `tea set-nfc on` writes one",
                false => "set — `tea set-nfc on` prints the tag's URL",
            });
        }
    }

    // The hub hearing from tea, as opposed to tea hearing from the hub above.
    // Its own section because it is its own switch: it works with the tag off.
    let ha = &nfc.home_assistant;
    if ha.publishes() || ha.publish_misconfigured().is_some() {
        section(&s, "home assistant");
        match ha.publish_misconfigured() {
            Some(why) => field(&s, "reports", "off", &why),
            None => {
                field(&s, "reports", "on", &format!("as {} on {}", ha.publish_entity.trim(), ha.url.trim()));
                field(&s, "fires", "", &format!("`{}` events — break_start, scan, released, break_end…", crate::publish::EVENT));
                field(&s, "hub token", "", &match ha.secret() {
                    Ok(_) => format!("from {}", tilde(std::path::Path::new(&ha.secret_source()))),
                    Err(why) => format!("MISSING — {why}"),
                });
            }
        }
    }

    section(&s, "change a setting");
    columns(&s, &["tea set-work 30m", "tea set-break 5m", "tea set-warn 30s"]);
    columns(&s, &["tea set-sound <file>", "tea set-nfc on|off"]);
    println!();
    println!("    {}", s.dim("editing the file by hand works too — then: tea reload"));
    println!("    {}", s.dim("tea run previews the page, re-reading the file each time"));
    println!();
}

/// Where values end and notes begin. Every note on the page starts here, in
/// every section, so the eye has one column to follow.
const LABEL: usize = 20;
const VALUE: usize = 7;

fn section(s: &Style, title: &str) {
    let rule = WIDTH.saturating_sub(title.chars().count() + 1);
    println!();
    println!("  {} {}", s.bold(title), s.dim(&"─".repeat(rule)));
}

/// Pad first, colour second: escape codes count as characters to `format!`, so
/// styling before padding silently wrecks the alignment.
fn field(s: &Style, label: &str, value: &str, note: &str) {
    let head = format!("    {}{}", format!("{label:<LABEL$}"), s.bold(&format!("{value:>VALUE$}")));
    if note.is_empty() {
        println!("{head}");
    } else {
        println!("{head}   {}", s.dim(note));
    }
}

fn columns(s: &Style, items: &[&str]) {
    let line: String = items.iter().map(|i| format!("{i:<21}")).collect();
    println!("    {}", s.dim(line.trim_end()));
}

fn basename(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

fn named(p: &Path) -> String {
    if p.as_os_str().is_empty() { "none".to_string() } else { basename(p) }
}


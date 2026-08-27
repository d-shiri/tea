//! `tea config` and `tea set-*` — reading and changing settings without
//! opening an editor.

use crate::config::{self, FileConfig, human, literal};
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

pub fn show(cfg: &Config, file: &FileConfig, path: &Path) {
    let s = Style::new();

    println!();
    heading(&s, "tea settings", &tilde(path));

    section(&s, "timing");
    field(&s, "work", &human(cfg.work), "before a break falls due");
    field(&s, "break", &human(cfg.brk), "how long you rest");
    field(&s, "warning", &human(cfg.warn_before), "heads-up before it appears");

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

    section(&s, "change a setting");
    columns(&s, &["tea set-work 30m", "tea set-break 5m", "tea set-warn 30s"]);
    columns(&s, &["tea set-sound <file>"]);
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


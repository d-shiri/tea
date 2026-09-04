//! Strict hold: the desktop's ways out of the break page, switched off for
//! the length of it.
//!
//! On GNOME every shortcut is a dconf setting, and a process in the session
//! can change them and see them take effect at once. So while a strict page
//! is up, the Super key, the overview, the app grid, Alt-Tab and its cousins,
//! the workspace switches, the dock's number keys and the hot corner are all
//! set to nothing, and when the page comes down they are put back exactly as
//! they were. Ctrl-Alt-F3 and the power button are below the desktop and
//! stay; the point is that leaving takes a decision, not that it is impossible.
//!
//! The one thing this must never do is leave somebody without a Super key.
//! What was there is written to a file *before* anything is changed, and the
//! file is read back and honoured at every start of tea, so a daemon that was
//! killed mid-break puts the keyboard right the moment it is next run.

use gtk::gio;
use gtk::gio::prelude::*;
use gtk::glib;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One setting as it was, in a form that survives a restart: the schema and
/// key that name it, and the value the way GVariant prints it, with its type
/// beside it so it can be read back into the same shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Saved {
    schema: String,
    key: String,
    kind: String,
    value: String,
}

/// Everything switched off. Schemas an installation lacks -- the dock is an
/// extension, not everybody has it -- are skipped, as is any key whose type
/// this does not know how to blank.
fn targets() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut add = |schema: &str, keys: &[&str]| {
        for key in keys {
            out.push((schema.to_string(), (*key).to_string()));
        }
    };
    // The Super key itself, and the corner of the screen that does the same.
    add("org.gnome.mutter", &["overlay-key"]);
    add("org.gnome.desktop.interface", &["enable-hot-corners"]);
    // The shell: overview, app grid, and the dock's Super+1..9.
    let mut shell = vec!["toggle-overview".to_string(), "toggle-application-view".to_string()];
    for n in 1..=9 {
        shell.push(format!("switch-to-application-{n}"));
        shell.push(format!("open-new-window-application-{n}"));
    }
    let shell: Vec<&str> = shell.iter().map(String::as_str).collect();
    add("org.gnome.shell.keybindings", &shell);
    // The window manager: every way of getting another window or workspace
    // in front. The lock screen is deliberately not here.
    let mut wm: Vec<String> = [
        "switch-applications",
        "switch-applications-backward",
        "switch-windows",
        "switch-windows-backward",
        "switch-group",
        "switch-group-backward",
        "cycle-windows",
        "cycle-windows-backward",
        "cycle-group",
        "cycle-group-backward",
        "switch-panels",
        "switch-panels-backward",
        "cycle-panels",
        "cycle-panels-backward",
        "switch-to-workspace-left",
        "switch-to-workspace-right",
        "switch-to-workspace-up",
        "switch-to-workspace-down",
        "switch-to-workspace-last",
        "move-to-workspace-left",
        "move-to-workspace-right",
        "move-to-workspace-up",
        "move-to-workspace-down",
        "show-desktop",
        "panel-main-menu",
    ]
    .iter()
    .map(|k| k.to_string())
    .collect();
    for n in 1..=4 {
        wm.push(format!("switch-to-workspace-{n}"));
    }
    let wm: Vec<&str> = wm.iter().map(String::as_str).collect();
    add("org.gnome.desktop.wm.keybindings", &wm);
    // Ubuntu's dock: one switch for all its number keys.
    add("org.gnome.shell.extensions.dash-to-dock", &["hot-keys"]);
    out
}

/// Where the originals wait while the page is up. Its existence is the fact
/// that something is switched off.
fn ledger() -> Option<PathBuf> {
    crate::state::dir().map(|d| d.join("held-keys.json"))
}

/// The settings object for a schema, or nothing if this desktop has no such
/// schema -- which is not an error, just a key that is not there to switch off.
fn settings(schema: &str) -> Option<gio::Settings> {
    let source = gio::SettingsSchemaSource::default()?;
    let schema = source.lookup(schema, true)?;
    Some(gio::Settings::new_full(&schema, gio::SettingsBackend::NONE, None))
}

/// The nothing of a given type: no key, no keys, no corner.
fn blank(kind: &str) -> Option<glib::Variant> {
    Some(match kind {
        "s" => "".to_variant(),
        "as" => Vec::<String>::new().to_variant(),
        "b" => false.to_variant(),
        _ => return None,
    })
}

/// Switch everything off, having first written down what it was. Comes back
/// with how many settings went.
///
/// Anything still written down from an earlier page -- a daemon killed
/// mid-break -- is put back first, so what gets written down now is what
/// the user actually had and not the nothing the last page left.
pub fn engage() -> Result<usize, String> {
    release()?;
    let mut saved = Vec::new();
    let mut changes: Vec<(gio::Settings, String, glib::Variant)> = Vec::new();
    for (schema, key) in targets() {
        let Some(settings) = settings(&schema) else { continue };
        if !settings.settings_schema().is_some_and(|s| s.has_key(&key)) {
            continue;
        }
        let value = settings.value(&key);
        let kind = value.type_().as_str().to_string();
        let Some(empty) = blank(&kind) else { continue };
        if value == empty {
            // Already nothing; leaving it alone means not "restoring" it to
            // nothing later either, which is the same thing.
            continue;
        }
        saved.push(Saved { schema, key: key.clone(), kind, value: value.print(true).to_string() });
        changes.push((settings, key, empty));
    }
    if saved.is_empty() {
        return Ok(0);
    }
    // The ledger goes to disk before a single setting moves.
    let path = ledger().ok_or("cannot locate a state directory: set XDG_STATE_HOME or HOME")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(&saved).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;

    let mut n = 0;
    for (settings, key, empty) in &changes {
        if settings.set_value(key, empty).is_ok() {
            n += 1;
        }
    }
    gio::Settings::sync();
    Ok(n)
}

/// Put back whatever the ledger says was switched off, and clear the ledger.
/// Nothing written down, nothing to do: `Ok(0)`.
pub fn release() -> Result<usize, String> {
    let Some(path) = ledger() else { return Ok(0) };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let saved: Vec<Saved> = serde_json::from_str(&text)
        .map_err(|e| format!("{} is not the ledger tea wrote: {e}", path.display()))?;
    let mut n = 0;
    let mut failed = Vec::new();
    for item in &saved {
        let restored = settings(&item.schema)
            .and_then(|settings| {
                let ty = glib::VariantTy::new(&item.kind).ok()?;
                let value = glib::Variant::parse(Some(ty), &item.value).ok()?;
                settings.set_value(&item.key, &value).ok()
            })
            .is_some();
        if restored {
            n += 1;
        } else {
            failed.push(format!("{}.{}", item.schema, item.key));
        }
    }
    gio::Settings::sync();
    if !failed.is_empty() {
        // The ledger stays, so the next start tries again and the user is
        // never left to work out which key it was.
        return Err(format!("could not put back {} — the originals are in {}", failed.join(", "), path.display()));
    }
    std::fs::remove_file(&path).map_err(|e| format!("cannot remove {}: {e}", path.display()))?;
    Ok(n)
}

/// At start-up: a ledger left by a page that never came down is a keyboard
/// still missing its Super key, and that is fixed before anything else.
pub fn restore_leftovers() {
    match release() {
        Ok(0) => {}
        Ok(n) => println!("tea: put {n} keyboard shortcuts back — a strict break was cut short"),
        Err(e) => eprintln!("tea: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_written_down_is_nothing_to_put_back() {
        // A scratch state directory with no ledger in it.
        let dir = std::env::temp_dir().join(format!("tea-strict-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: tests in this module run single-threaded with respect to this
        // variable; nothing else reads it concurrently.
        unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };
        assert_eq!(release().unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_target_is_a_type_this_knows_how_to_blank() {
        // The real schemas, on a machine that has them. Elsewhere the test
        // has nothing to check and says so by passing.
        for (schema, key) in targets() {
            let Some(settings) = settings(&schema) else { continue };
            if !settings.settings_schema().is_some_and(|s| s.has_key(&key)) {
                panic!("{schema} has no key {key}");
            }
            let kind = settings.value(&key).type_().as_str().to_string();
            assert!(blank(&kind).is_some(), "{schema}.{key} is a {kind}, which cannot be blanked");
        }
    }

    /// Touches the live session: switches the shortcuts off and puts them
    /// back within a second. Run by hand with `--ignored`.
    #[test]
    #[ignore]
    fn the_shortcuts_go_and_come_back_exactly() {
        let before: Vec<(String, String, String)> = targets()
            .into_iter()
            .filter_map(|(s, k)| settings(&s).map(|st| (s, k.clone(), st.value(&k).print(true).to_string())))
            .collect();
        let gone = engage().unwrap();
        assert!(gone > 0);
        let overlay = settings("org.gnome.mutter").unwrap().value("overlay-key");
        assert_eq!(overlay.get::<String>().unwrap(), "");
        // Seen from outside this process, which is what tells a dconf write
        // from one that only went to an in-memory backend.
        assert_eq!(outside("org.gnome.mutter", "overlay-key"), "''");
        assert!(ledger().unwrap().exists());
        assert_eq!(release().unwrap(), gone);
        assert!(!ledger().unwrap().exists());
        for (s, k, v) in &before {
            assert_eq!(settings(s).unwrap().value(k).print(true).to_string(), *v, "{s}.{k}");
        }
        assert_eq!(outside("org.gnome.mutter", "overlay-key"), "'Super_L'");

        // A daemon killed mid-break: the ledger is there, the keys are gone,
        // and the next start puts them back from the ledger alone.
        engage().unwrap();
        assert_eq!(outside("org.gnome.mutter", "overlay-key"), "''");
        restore_leftovers();
        assert!(!ledger().unwrap().exists());
        assert_eq!(outside("org.gnome.mutter", "overlay-key"), "'Super_L'");
        for (s, k, v) in &before {
            assert_eq!(settings(s).unwrap().value(k).print(true).to_string(), *v, "{s}.{k}");
        }
    }

    fn outside(schema: &str, key: &str) -> String {
        let out = std::process::Command::new("gsettings").args(["get", schema, key]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }
}

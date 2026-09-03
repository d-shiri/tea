//! The settings page: this config file, in a browser, on the daemon's own
//! port.
//!
//! Nothing new listens. The ear that hears the tag already answers HTTP on
//! `[port]`, behind its token, so the page is two more paths on it: one
//! that serves the HTML, one that reads and writes the file. The page itself
//! is a few hundred lines of plain HTML and script with no library behind it,
//! and it costs the daemon nothing until somebody opens it -- a socket that
//! nobody connects to is a socket the main loop never wakes up for.
//!
//! Off by default. An upgrade must never quietly put a file editor on a port.

use crate::config::{self, FileConfig};
use crate::nfc::Mode;
use gtk::glib;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `[settings]` in the config file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// Serve the page at all. `"on"` or `"off"`.
    pub page: Mode,
}

impl Config {
    pub fn on(&self) -> bool {
        self.page == Mode::On
    }
}

/// What the ear needs in order to serve the page: where the file is.
#[derive(Debug, Clone)]
pub struct Site {
    pub path: PathBuf,
}

/// The page, built into the binary so there is nothing to install beside it.
pub const PAGE: &str = include_str!("settings.html");

/// Where a browser finds the page: the listening address with the token in
/// the query, because a browser bar has nowhere else to put one. `0.0.0.0`
/// is an address to listen on, not one to open.
pub fn url(nfc: &crate::nfc::Config) -> String {
    let host = match nfc.listen.split_once(':') {
        Some(("0.0.0.0" | "", port)) | Some(("[::]", port)) => format!("127.0.0.1:{port}"),
        _ => nfc.listen.clone(),
    };
    format!("http://{host}/settings?token={}", nfc.token)
}

/// The file as it is on disk.
pub fn read(site: &Site) -> Result<String, String> {
    std::fs::read_to_string(&site.path).map_err(|e| format!("cannot read {}: {e}", site.path.display()))
}

/// Replace the file with what the page sent, if it would load.
///
/// The same parse the daemon does at start-up and the same reconciliation, so
/// a file that survives this survives boot; nothing is written otherwise. The
/// write goes through a rename, so a crash mid-way leaves the old file, not
/// half of a new one.
pub fn write(site: &Site, text: &str) -> Result<(), String> {
    let parsed: FileConfig =
        toml::from_str(text).map_err(|e| format!("not saved — that would not load:\n{e}"))?;
    let mut cfg: tea_core::Config = parsed.into();
    config::reconcile(&mut cfg).map_err(|e| format!("not saved — {e}"))?;
    replace(&site.path, text)
}

fn replace(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
}

/// The restart `tea reload` would do, a moment from now so the reply gets out
/// of the door first. Only under systemd, where there is something to bring
/// the daemon back; run by hand, the file is saved and the restart is yours.
pub fn restart_soon() -> bool {
    if std::env::var_os("INVOCATION_ID").is_none() {
        return false;
    }
    glib::timeout_add_local_once(Duration::from_millis(300), || {
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "--no-block", "restart", "tea.service"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    });
    true
}

/// `tea settings`: whatever it takes to have the page, then the page.
///
/// A new install has the page off and no token, and the page is the thing
/// meant to spare people the file -- so this writes the switch and the token
/// itself, restarts the service so it hears about them, and opens the
/// browser. On a machine where it is all in place already, it just opens.
pub fn open(path: &Path) {
    let did = crate::settings::enable_page(path).unwrap_or_else(|e| crate::fail(&e));
    for line in &did {
        println!("tea: {line}");
    }
    if !did.is_empty() {
        // The daemon reads the file once, at start. Bring it round the same
        // way `tea reload` does; where that is not possible, say so and
        // still open the page -- it will sit there until the daemon is back.
        match std::process::Command::new("systemctl")
            .args(["--user", "restart", "tea.service"])
            .status()
        {
            Ok(status) if status.success() => println!("tea: reloaded — the page is live"),
            _ => println!("tea: restart tea to pick that up"),
        }
    }
    let file = config::load(path).unwrap_or_else(|e| crate::fail(&e));
    let url = url(&file.nfc);
    println!("tea: {url}");
    match std::process::Command::new("xdg-open")
        .arg(&url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(_) => println!("tea: opened in your browser"),
        Err(_) => println!("tea: nothing here to open it with — the address is yours to open"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site() -> (tempdir::Dir, Site) {
        let dir = tempdir::Dir::new("tea-web");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "work = \"30m\"\n").unwrap();
        (dir, Site { path })
    }

    #[test]
    fn a_file_that_would_not_load_is_not_written() {
        let (_dir, site) = site();
        let err = write(&site, "work = \"30m\"\nbanana = 1\n").unwrap_err();
        assert!(err.contains("not saved"), "{err}");
        assert_eq!(read(&site).unwrap(), "work = \"30m\"\n", "the old file is untouched");

        // Parses, but cannot work: the same refusal `tea set-work 0` gets.
        let err = write(&site, "work = \"0s\"\n").unwrap_err();
        assert!(err.contains("greater than zero"), "{err}");
    }

    #[test]
    fn a_file_that_loads_replaces_the_old_one() {
        let (_dir, site) = site();
        write(&site, "work = \"25m\"   # kept\n[settings]\npage = \"on\"\n").unwrap();
        assert_eq!(read(&site).unwrap(), "work = \"25m\"   # kept\n[settings]\npage = \"on\"\n");
        assert!(!site.path.with_extension("tmp").exists(), "nothing left behind");
    }

    #[test]
    fn the_address_is_one_a_browser_can_open() {
        let mut nfc = crate::nfc::Config::default();
        nfc.token = "abc".into();
        nfc.listen = "0.0.0.0:9797".into();
        assert_eq!(url(&nfc), "http://127.0.0.1:9797/settings?token=abc");
        nfc.listen = "192.168.2.7:9797".into();
        assert_eq!(url(&nfc), "http://192.168.2.7:9797/settings?token=abc");
    }

    /// A directory that goes away with the test. Small enough not to be worth
    /// a dependency.
    mod tempdir {
        use std::path::{Path, PathBuf};
        pub struct Dir(PathBuf);
        impl Dir {
            pub fn new(tag: &str) -> Self {
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                let dir = std::env::temp_dir().join(format!("{tag}-{}-{nanos}", std::process::id()));
                std::fs::create_dir_all(&dir).unwrap();
                Dir(dir)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}

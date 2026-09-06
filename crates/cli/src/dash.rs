//! `tea dash` — the last three weeks, as a page.
//!
//! A file first, and a served page only where one is already being served. The
//! page is built here and written to disk: nothing is asked of the network and
//! no library is fetched from anywhere, so it can be copied to a phone, mailed
//! to yourself, or opened on a plane, and no port on this machine exists
//! because you wanted to look at a chart. What that file cannot do is *write*
//! anything back -- a `file://` page cannot edit your config.
//!
//! Where the settings page is switched on there is a port already, and the
//! daemon serves the same page at `/dash` behind the same token, so the two
//! pages are one link apart and the numbers are gathered per request rather
//! than frozen at the moment the file was written. See [`page`]. It stays off
//! wherever the settings page is off; a dashboard is not a reason to open a
//! port that was not open.
//!
//! `tea dash` writes the file either way, and opens that served copy in
//! preference to it wherever the daemon is up to answer -- the file has no
//! token, so it is the one copy of this page with no way back to your settings
//! and no way to refresh itself. See [`show`].
//!
//! The numbers come from two places. Today's are read from the state file, the
//! same way `tea status` reads them; everything older comes from the history
//! log, which only exists from the moment a service new enough to write it
//! started running. Nothing can reconstruct the weeks before that, so the page
//! says as much rather than drawing empty axes.

use crate::config::{self, FileConfig};
use crate::history;
use crate::state;
use crate::status;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The page, with a hole in it where the numbers go.
const TEMPLATE: &str = include_str!("dash.html");

/// The hole. A `const` rather than a literal so that renaming it in the HTML
/// and forgetting to rename it here is caught by a test rather than by a blank
/// page a week later.
const PLACEHOLDER: &str = "/*__TEA_DATA__*/ null";

/// Beyond this the state file is not being refreshed and nothing is running.
/// The same window `tea status` uses; they are answering the same question.
const FRESH: Duration = Duration::from_secs(15);

/// Build the page, write it, and hand it to the browser.
pub fn show(file: &FileConfig, cfg: &tea_core::Config, config_path: &Path, boottime: Duration, open: bool) {
    let data = gather(file, cfg, config_path, boottime);
    let page = render(TEMPLATE, &data);

    let Some(out) = out_path() else {
        crate::fail("cannot locate a state directory: set XDG_STATE_HOME or HOME")
    };
    if let Err(e) = write(&out, &page) {
        crate::fail(&e)
    }

    println!("tea: {}", out.display());
    if !open {
        return;
    }

    // Two copies of the same page exist, and only one of them can go anywhere.
    // A file cannot carry the token -- deliberately, because this file gets
    // mailed and screenshotted, and see `redact` -- so the file copy has no
    // link to the settings page and no way to refresh itself. Where the daemon
    // is up and already serving, that copy is the better one to hand over: the
    // same numbers gathered per request, with both links live. The file is
    // still written and still printed above; it is the copy you keep.
    let live = served_copy(file, data["running"].as_bool().unwrap_or(false));
    if let Some(url) = &live {
        println!("tea: {url}");
        println!("tea: opening the served copy — it refreshes, and it links to your settings");
    }
    let target = live.unwrap_or_else(|| out.display().to_string());

    // Detached on purpose, and its failure is not this command's failure: over
    // SSH, or on a machine with no desktop, there is nothing to open it with
    // and the path printed above is the whole of the answer.
    match std::process::Command::new("xdg-open")
        .arg(&target)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(_) => println!("tea: opened in your browser"),
        Err(_) => println!("tea: nothing here to open it with — the file is yours to open"),
    }
}

/// The live copy to hand over instead of the file, where there is one.
///
/// Two things have to be true: the daemon is up (a URL for a service that is
/// not answering is worse than a file that opens), and it is serving the page
/// at all -- which is the settings page's switch, and a token to get past the
/// door. Anything else and the file is the answer.
fn served_copy(file: &FileConfig, running: bool) -> Option<String> {
    (running && crate::web::serving(file)).then(|| crate::web::dash(&file.nfc))
}

/// The same page, built from whatever is on disk right now and handed back
/// rather than written.
///
/// This is what the daemon serves at `/dash`, beside the settings page and
/// behind the same token. `tea dash` writes a file and opens it, which is a
/// snapshot of the moment it ran; served, the numbers are gathered per request,
/// which is what makes a link between the two pages worth following.
///
/// A config that will not parse is not this page's problem to report: it draws
/// the defaults and the settings card says so, the same as it does on a machine
/// that has no config file yet.
pub fn page(config_path: &Path, boottime: Duration) -> String {
    let file = match config_path.exists() {
        true => config::load(config_path).unwrap_or_default(),
        false => FileConfig::default(),
    };
    let mut cfg: tea_core::Config = file.clone().into();
    let _ = crate::config::reconcile(&mut cfg);
    render(TEMPLATE, &gather(&file, &cfg, config_path, boottime))
}

/// `$XDG_STATE_HOME/tea/dash.html`, beside the log it is drawn from.
fn out_path() -> Option<PathBuf> {
    state::dir().map(|d| d.join("dash.html"))
}

/// Everything the page needs, as one JSON value.
fn gather(
    file: &FileConfig,
    cfg: &tea_core::Config,
    config_path: &Path,
    boottime: Duration,
) -> serde_json::Value {
    let saved = state::Store::new().and_then(|store| store.load(boottime));
    let running = saved.as_ref().is_some_and(|s| s.gap <= FRESH);
    let today = crate::clock::today();
    let tally = saved.map(|s| {
        let mut t = s.tally;
        // Yesterday's numbers are not today's, and the daemon may not have
        // ticked since midnight to say so.
        t.roll(&today);
        t
    });

    let raw = std::fs::read_to_string(config_path).unwrap_or_default();

    // Sorted rather than trusted to be in order: appends are chronological, but
    // a log that has been trimmed, concatenated or hand-edited should still draw
    // left to right instead of drawing a scribble.
    let mut events = history::read();
    events.sort_by_key(|e| e.at());

    json!({
        "generated": crate::clock::unix_now(),
        "today": today,
        "running": running,
        "tally": tally.map(|t| json!({
            "breaks": t.breaks,
            "credited": t.credited,
            "postponed": t.postponed,
            "steps": t.steps,
            "chores": t.chores,
            "cheats": t.cheats,
            "worked_min": t.worked_ms / 60_000,
        })),
        "settings": {
            "work": cfg.work.as_secs(),
            "brk": cfg.brk.as_secs(),
            "long_every": cfg.long_every,
            "long_brk": cfg.long_brk.as_secs(),
            "nfc": file.nfc.on(),
            // The dashed line on the steps chart. Zero when no walk is asked
            // for, and the chart draws no line rather than a line at nothing.
            "steps_wanted": if file.nfc.counts_steps() { file.nfc.steps.count } else { 0 },
            "hours": if file.hours.set() { file.hours.describe() } else { "always".to_string() },
            // The hub's own address, so the header can link to it the way the
            // settings page does — a service of its own, on its own address,
            // needing nothing of tea's to reach, which is why that one link
            // works from the file copy too.
            //
            // The address and nothing else. The token is not here and must
            // never be: this page is written to be mailed and screenshotted,
            // which is the whole reason `redact` exists below.
            "hub": file.nfc.home_assistant.url.trim(),
            // Whether there is a settings page to point at. The file copy of
            // this page cannot link to it -- no token -- but it can say what
            // opens it, and saying that on a machine with the page switched
            // off would be pointing at a door that is not there.
            "settings_page": crate::web::serving(file),
        },
        "config": {
            "path": status::tilde(config_path),
            "text": redact(&raw),
        },
        "events": events,
    })
}

/// Put the numbers in the page.
///
/// `<` is escaped on the way in. Everything embedded here is text somebody can
/// edit -- a config comment, a spoken phrase, a path -- and a `</script>` in any
/// of it would end the block early and leave the rest of the page as prose.
fn render(template: &str, data: &serde_json::Value) -> String {
    let json = data.to_string().replace('<', "\\u003c");
    template.replace(PLACEHOLDER, &json)
}

/// Blank out anything called `token`, keeping the line it lived on.
///
/// The config file is `chmod 600` and `tea set-nfc` works to keep it that way.
/// A dashboard that copies the token into a second file -- one that gets opened
/// in a browser, and screenshotted, and sent to somebody -- would be the thing
/// that undid all of that.
fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        match line.split_once('=') {
            Some((key, _)) if key.trim() == "token" => {
                let indent = &line[..line.len() - line.trim_start().len()];
                out.push_str(indent);
                out.push_str("token = \"(hidden — see your config file)\"");
            }
            _ => out.push_str(line),
        }
        out.push('\n');
    }
    out
}

/// Written through a temporary file and renamed over, and readable only by you:
/// the config text it carries came out of a file that is `chmod 600`.
fn write(path: &Path, page: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, page).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file has no token in it, so the file copy of this page is the one copy
    /// with no link to the settings page and no way to refresh itself. Where
    /// the daemon is up and already serving, `tea dash` hands over that copy
    /// instead -- but only where both of those hold.
    #[test]
    fn the_served_copy_is_preferred_only_where_there_is_one() {
        let mut file = FileConfig::default();
        assert_eq!(served_copy(&file, true), None, "no settings page, no served copy");

        file.settings.page = crate::nfc::Mode::On;
        assert_eq!(served_copy(&file, true), None, "a page with no token is a door nobody opens");

        file.nfc.token = "abc".into();
        file.nfc.listen = "127.0.0.1:9797".into();
        assert_eq!(
            served_copy(&file, true).as_deref(),
            Some("http://127.0.0.1:9797/dash?token=abc"),
            "switched on, with a token, and answering"
        );
        assert_eq!(
            served_copy(&file, false),
            None,
            "a URL for a daemon that is not answering is worse than a file that opens"
        );
    }

    #[test]
    fn the_page_says_where_the_settings_are_even_when_it_cannot_link_there() {
        assert!(
            TEMPLATE.contains("SET.settings_page"),
            "the file copy no longer reads whether there is a settings page to name"
        );
        assert!(
            TEMPLATE.contains("tea settings"),
            "and no longer names the command that opens it"
        );
    }

    #[test]
    fn the_page_has_somewhere_to_put_the_numbers() {
        assert!(
            TEMPLATE.contains(PLACEHOLDER),
            "dash.html no longer has {PLACEHOLDER:?} in it — renaming it here and there \
             separately gives a page that opens and draws nothing"
        );
    }

    #[test]
    fn a_config_comment_cannot_end_the_script_block() {
        let data = json!({ "config": { "text": "# see </script><h1>hello\n" } });
        let page = render("const DATA = /*__TEA_DATA__*/ null;", &data);
        assert!(!page.contains("</script>"), "the tag survived: {page}");
        assert!(page.contains("\\u003c/script>"), "and it should still be readable: {page}");
    }

    /// The hub's address belongs on the page — it is what the link in the
    /// header is made of, and it is already in the config quoted at the foot.
    /// Its token does not, ever: this page is written to be mailed,
    /// screenshotted and opened on a phone.
    #[test]
    fn the_hubs_address_reaches_the_page_and_its_token_does_not() {
        let dir = std::env::temp_dir().join(format!("tea-hub-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "[nfc]\nmode = \"on\"\n\n\
             [nfc.home_assistant]\n\
             url = \"http://hub.local:8123\"\n\
             token = \"s3cret-hub-token\"\n\
             entity = \"tag.front_door\"\n",
        )
        .unwrap();

        let file = config::load(&path).unwrap();
        let mut cfg: tea_core::Config = file.clone().into();
        let _ = config::reconcile(&mut cfg);
        let data = gather(&file, &cfg, &path, Duration::ZERO);

        assert_eq!(
            data["settings"]["hub"], "http://hub.local:8123",
            "the address is there, so the link in the header works"
        );
        assert!(
            !data.to_string().contains("s3cret-hub-token"),
            "the token reached the page: {data}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_token_never_reaches_the_page() {
        let text = "[nfc]\nmode = \"on\"\n  token = \"deadbeefdeadbeef\"\ngrace = \"10m\"\n";
        let out = redact(text);
        assert!(!out.contains("deadbeef"), "{out}");
        assert!(out.contains("  token = "), "the line stays, so the file still reads as a file");
        assert!(out.contains("grace = \"10m\""), "and nothing else is touched");
    }

    #[test]
    fn a_key_merely_ending_in_token_is_left_alone() {
        let out = redact("token_file = \"~/.config/tea/.env\"\n");
        assert!(out.contains("token_file = \"~/.config/tea/.env\""), "{out}");
    }

    #[test]
    fn the_numbers_actually_land_in_the_page() {
        let page = render("const DATA = /*__TEA_DATA__*/ null;", &json!({ "breaks": 3 }));
        assert_eq!(page, r#"const DATA = {"breaks":3};"#);
    }
}

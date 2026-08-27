//! Noise when a break starts and ends.
//!
//! Spawned as short-lived processes rather than linked in: the system already
//! has players that respect the user's output device and volume, and getting
//! audio wrong is a good way to make a break tool hateful. Every failure here
//! is silent-and-carry-on — a missing player must never stop a break.

use serde::Deserialize;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Off,
    /// A short sound from the desktop sound theme.
    Chime,
    /// Spoken, through the system's screen-reader voice.
    Voice,
    Both,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub mode: Mode,
    pub start_file: PathBuf,
    pub end_file: PathBuf,
    pub start_words: String,
    pub end_words: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // Silent until told otherwise. A sound you did not choose, played
            // at you several times an hour, is worse than no sound at all.
            mode: Mode::Off,
            start_file: PathBuf::new(),
            end_file: PathBuf::new(),
            start_words: "Time for a break".into(),
            end_words: "Break over".into(),
        }
    }
}

/// `paplay` and `pw-play` go through libsndfile: WAV, OGG and FLAC only. Hand
/// either of them an MP3 and it fails silently, which is the worst possible
/// outcome for a sound you configured on purpose.
const SIMPLE: &[&str] = &["paplay", "pw-play"];
const SIMPLE_FORMATS: &[&str] = &["wav", "ogg", "oga", "flac", "aiff", "au"];
/// Real decoders, for everything else.
const DECODERS: &[&str] = &["ffplay", "mpv", "mpg123"];
const SPEAKER: &str = "spd-say";

pub struct Player {
    cfg: Config,
    can_speak: bool,
    /// Finished children, waiting to be reaped. Left unreaped they would
    /// accumulate as zombies for the life of the daemon.
    pending: Vec<Child>,
}

impl Player {
    pub fn new(cfg: Config) -> Self {
        let wants_chime = matches!(cfg.mode, Mode::Chime | Mode::Both);
        let wants_voice = matches!(cfg.mode, Mode::Voice | Mode::Both);

        if wants_chime {
            for file in [&cfg.start_file, &cfg.end_file] {
                // An unset file means "no sound at this moment", which is a
                // choice, not a mistake.
                if file.as_os_str().is_empty() {
                    continue;
                }
                if !file.exists() {
                    eprintln!("tea: sound file not found: {}", file.display());
                } else if command_for(file).is_none() {
                    eprintln!(
                        "tea: no player can handle {} (looked for {}, {})",
                        file.display(),
                        SIMPLE.join(", "),
                        DECODERS.join(", ")
                    );
                }
            }
        }

        let can_speak = wants_voice && on_path(&SPEAKER);
        if wants_voice && !can_speak {
            eprintln!("tea: {SPEAKER} not found; breaks will not be spoken");
        }

        Self { cfg, can_speak, pending: Vec::new() }
    }

    pub fn break_starts(&mut self) {
        let (file, words) = (self.cfg.start_file.clone(), self.cfg.start_words.clone());
        self.emit(&file, &words);
    }

    pub fn break_ends(&mut self) {
        let (file, words) = (self.cfg.end_file.clone(), self.cfg.end_words.clone());
        self.emit(&file, &words);
    }

    fn emit(&mut self, file: &std::path::Path, words: &str) {
        self.reap();
        if matches!(self.cfg.mode, Mode::Chime | Mode::Both)
            && !file.as_os_str().is_empty()
            && let Some(mut cmd) = command_for(file)
        {
            self.spawn(cmd.arg(file));
        }
        if self.can_speak && !words.trim().is_empty() {
            self.spawn(Command::new(SPEAKER).arg(words));
        }
    }

    fn spawn(&mut self, cmd: &mut Command) {
        match cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
            Ok(child) => self.pending.push(child),
            // Deliberately quiet: this is decoration, and the break itself has
            // already happened by the time anyone would read a warning.
            Err(_) => {}
        }
    }

    fn reap(&mut self) {
        self.pending.retain_mut(|c| !matches!(c.try_wait(), Ok(Some(_)) | Err(_)));
    }
}

/// Pick a player that can actually decode this file. Extension-based, because
/// the alternative is probing every file with every player at startup.
fn command_for(file: &std::path::Path) -> Option<Command> {
    let ext =
        file.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();

    if SIMPLE_FORMATS.contains(&ext.as_str())
        && let Some(player) = SIMPLE.iter().copied().find(on_path)
    {
        return Some(Command::new(player));
    }

    if let Some(player) = DECODERS.iter().copied().find(on_path) {
        let mut cmd = Command::new(player);
        match player {
            // Both of these are media players; tell them not to act like one.
            "ffplay" => cmd.args(["-nodisp", "-autoexit", "-loglevel", "quiet"]),
            "mpv" => cmd.args(["--no-video", "--really-quiet"]),
            _ => &mut cmd,
        };
        return Some(cmd);
    }

    // Last resort: the simple players might cope with an unfamiliar extension.
    SIMPLE.iter().copied().find(on_path).map(Command::new)
}

fn on_path(name: &&str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| dir.join(name).is_file())
    })
}

//! Pure break-scheduling state machine.
//!
//! No clock, no I/O, no platform bindings: the caller feeds it elapsed time and
//! a measured idle duration, and it answers with commands. That keeps every
//! interesting rule — idle credit, deferral, postpone budget — unit-testable
//! without a compositor or a five-minute wait.

use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Work this long before a break falls due.
    pub work: Duration,
    /// How long the break lasts.
    pub brk: Duration,
    /// Heads-up warning this long before the break starts.
    pub warn_before: Duration,
    /// Idle at least this long counts as a break already taken.
    pub idle_credit: Duration,
    /// Idle at least this long stops the work timer accumulating.
    pub idle_pause: Duration,
    /// How much time one postpone buys.
    pub postpone: Duration,
    /// Postpones allowed per `postpone_window`.
    pub postpone_budget: u32,
    pub postpone_window: Duration,
    /// If a call or video holds a break up for longer than this, say so.
    /// Zero disables the warning.
    pub defer_warn_after: Duration,
    /// Serving the time is no longer enough on its own: the break also waits
    /// for a release signal from somewhere you have to get up to reach.
    pub require_release: bool,
    /// Stop waiting for that signal after this long and end the break anyway.
    /// Zero waits for as long as it takes. The point of the gate is to get you
    /// out of the chair, not to hold your desk hostage to a flat phone.
    pub release_grace: Duration,
    /// Every this-many-th break is a long one. Zero means they are all the
    /// same length.
    ///
    /// Four five-minute breaks in a row are four chances to stand up and no
    /// chance to go anywhere. The long one is the walk, the coffee, the thing
    /// you cannot do in three hundred seconds.
    pub long_every: u32,
    /// How long that one lasts.
    pub long_brk: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            work: Duration::from_secs(25 * 60),
            brk: Duration::from_secs(5 * 60),
            warn_before: Duration::from_secs(30),
            idle_credit: Duration::from_secs(5 * 60),
            idle_pause: Duration::from_secs(60),
            postpone: Duration::from_secs(60),
            postpone_budget: 2,
            postpone_window: Duration::from_secs(60 * 60),
            defer_warn_after: Duration::from_secs(20 * 60),
            require_release: false,
            release_grace: Duration::from_secs(10 * 60),
            // Off by default like everything else that changes when your day is
            // interrupted: a longer break is a good idea, and an upgrade that
            // silently starts taking fifteen minutes off you is not.
            long_every: 0,
            long_brk: Duration::from_secs(15 * 60),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Accumulating work time. `due` means the break is owed but something is
    /// inhibiting it (screen share, call), so we are holding.
    Working { worked: Duration, due: bool },
    Breaking { rested: Duration },
}

/// What the host should do about it. The UI layer renders these; it makes no
/// scheduling decisions of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Break due in roughly this long.
    Warn { until_break: Duration },
    /// Break is owed but deferred by an inhibitor; emitted once per deferral.
    Deferred,
    ShowOverlay { duration: Duration },
    /// Countdown update while the overlay is up.
    Tick { remaining: Duration },
    HideOverlay,
    /// Time away was long enough to count as a break; the work timer reset.
    CreditedIdle { was_idle: Duration },
    /// A break has been waiting on an inhibitor for an unreasonable time --
    /// usually an app that forgot to say it was finished.
    Overdue { waiting: Duration },
    /// The time has been served and the break is now waiting on its release
    /// signal. Emitted once, when the countdown runs out.
    AwaitRelease,
    /// The signal never came. Ending the break on the clock alone.
    GaveUpWaiting { waited: Duration },
}

/// What a postpone would buy, and how many are left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snooze {
    pub duration: Duration,
    pub left: u32,
}

/// One line of the list the page shows in the corner: what it says, and
/// whether it has been ticked off.
///
/// Here rather than beside the hub code for the same reason [`Snooze`] is: it
/// is part of what a blocker is told, and the engine must not have to know
/// where a to-do list comes from.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Chore {
    pub summary: String,
    pub done: bool,
    /// When it is wanted by, if the list says. Already put into words, because
    /// the page draws it and the page does not own a calendar.
    pub due: Option<Due>,
}

/// A deadline as the corner shows it: a few characters -- `16:30`, `Fri`,
/// `4 Sep` -- and whether it has already gone by.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Due {
    pub label: String,
    pub late: bool,
    /// Within the hour, or gone by: close enough that the row should stir.
    pub soon: bool,
}

/// Whatever actually stands between you and the keyboard.
///
/// The scheduler decides *when*; this decides *how*. Keeping it a trait is what
/// lets the GTK overlay, a terminal fallback, and (if soft enforcement turns
/// out to be too easy to dodge) a GNOME Shell input grab be swapped without the
/// timing rules noticing.
pub trait Blocker {
    /// Put the block up for `total`.
    fn engage(&mut self, total: Duration);
    /// Countdown, once a second, while it is up.
    fn update(&mut self, remaining: Duration);
    /// Take it down.
    fn release(&mut self);
    /// A break is coming in `until_break`. `snooze` is `None` when the
    /// postpone budget is spent.
    ///
    /// The budget is passed in rather than read back out of the scheduler: the
    /// UI is called from inside the tick, so reaching back for state it already
    /// had would mean borrowing the scheduler twice.
    fn warn(&mut self, until_break: Duration, snooze: Option<Snooze>);
    /// The warning no longer applies (postponed, or the break arrived).
    fn clear_warning(&mut self) {}
    /// The countdown has run out but the break is not over: it is waiting to
    /// be released. Say so, or the page reads as a clock that has stuck.
    fn await_release(&mut self) {}
    /// Whether whatever watches for the release signal can be reached at all.
    ///
    /// Called on every tick of a gated break, so the page can say that scanning
    /// would not be noticed *before* someone walks off to do it. Idempotent:
    /// the same answer twice means nothing has changed.
    fn release_source(&mut self, _reachable: bool) {}
    /// The tag has been scanned for the break on screen.
    ///
    /// Not the same thing as the break being over: where a walk is counted too,
    /// the scan is only half the gate. The page still has to show it -- a scan
    /// that changes nothing on screen is a scan the user assumes did not work.
    fn tag_seen(&mut self) {}
    /// How the walk is going: `walked` steps counted since the page went up,
    /// out of the `needed` this break is asking for.
    ///
    /// `marked` says the count has been re-based since the page appeared -- the
    /// first report a phone sends mid-break carries steps from before it, so it
    /// moves the mark instead of paying for the break. The page has to be able
    /// to say that, or it reads *0 of 50* at somebody who has demonstrably just
    /// walked, and they walk it again.
    ///
    /// Called on every tick of a gated break, so it must do nothing when
    /// nothing has changed.
    fn steps_seen(&mut self, _walked: u32, _needed: u32, _marked: bool) {}
    /// How the time on your feet is going: `secs` counted in a moving state
    /// since the page went up, out of the `needed` this break asks for.
    /// `lost` says the sensor cannot be read, so the page can say that this
    /// half will not close by itself. Same contract as `steps_seen`: called
    /// every tick, must do nothing when nothing has changed.
    fn motion_seen(&mut self, _secs: u32, _needed: u32, _lost: bool) {}
    /// What to do with the break: a handful of jobs off a list kept elsewhere,
    /// and which of them are already done. `list` is what the list is called,
    /// for the page to put at the top of them.
    ///
    /// `hidden` is how many open jobs did not fit, so the page can say so
    /// rather than leave a count somewhere else contradicting the rows; and
    /// `today` is how many jobs have been ticked off during today's breaks,
    /// this one included, which is a fact about the day rather than about the
    /// rows and has to be shown as one.
    ///
    /// Not part of the gate and never called as if it were: an empty slice
    /// means there is nothing to show, which is what a page with no list
    /// configured, and a page whose hub has not answered yet, both look like.
    /// Called on every tick like the three above, so it must do nothing when
    /// nothing has changed.
    fn chores_seen(&mut self, _list: &str, _jobs: &[Chore], _hidden: u32, _today: u32) {}
    /// The steps arrived and the phone said `still` the whole time: the count
    /// came from a hand, not a walk. `busted` goes back to false the moment
    /// the phone reports moving, so the page can stop teasing. Called every
    /// tick of a gated break, same contract as the two above.
    fn cheat_seen(&mut self, _busted: bool) {}
    /// The release signal has arrived for the break currently on screen.
    ///
    /// Called by the host rather than emitted as a command, because a signal
    /// can land between ticks and the page has to show it either way: a scan
    /// that changes nothing on screen is a scan the user assumes did not work,
    /// and they walk back to the tag to do it again.
    fn release_seen(&mut self) {}
    /// Anything worth saying that isn't a state change.
    fn note(&mut self, _msg: &str) {}
}

/// Feed one tick to the scheduler and hand the results to a blocker.
pub fn drive(
    sched: &mut Scheduler,
    ui: &mut dyn Blocker,
    delta: Duration,
    idle: Duration,
    inhibited: bool,
) -> Vec<Command> {
    let commands = sched.tick(delta, idle, inhibited);
    for cmd in &commands {
        match *cmd {
            Command::Warn { until_break } => {
                let left = sched.postpones_left();
                let snooze =
                    (left > 0).then(|| Snooze { duration: sched.config().postpone, left });
                ui.warn(until_break, snooze);
            }
            Command::Deferred => ui.note("break owed, deferred by an inhibitor"),
            Command::ShowOverlay { duration } => {
                ui.clear_warning();
                ui.engage(duration);
            }
            Command::Tick { remaining } => ui.update(remaining),
            Command::HideOverlay => ui.release(),
            Command::AwaitRelease => ui.await_release(),
            Command::GaveUpWaiting { waited } => {
                ui.note(&format!("no release after {}s — ending the break anyway", waited.as_secs()))
            }
            Command::CreditedIdle { was_idle } => {
                ui.clear_warning();
                ui.note(&format!("away {}s — counted as your break", was_idle.as_secs()));
            }
            // Handled by the host, which can name the app responsible.
            Command::Overdue { .. } => {}
        }
    }
    commands
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostponeResult {
    Granted { remaining_budget: u32 },
    /// Nothing to postpone — no break is due or imminent.
    NotPending,
    Exhausted,
}

/// What a release signal did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseResult {
    /// The time was already served: the break ends on this tick.
    Freed,
    /// Banked. The break still has this long to run, and will end when it does
    /// without anything else being asked of you.
    Banked { remaining: Duration },
    /// Nothing to release — no break is on screen.
    NotBreaking,
    /// Breaks are not gated on a signal, so this one changes nothing.
    NotRequired,
}

/// Everything worth surviving a restart.
///
/// Plain data on purpose: core stays dependency-free, so the host picks the
/// serialisation format rather than having one baked in here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    pub breaking: bool,
    pub worked: Duration,
    pub rested: Duration,
    pub due: bool,
    pub postpones_used: u32,
    pub window_elapsed: Duration,
    /// A release signal already arrived for the break in progress.
    pub released: bool,
    /// How long the break has been sitting past its countdown waiting for one.
    pub waiting: Duration,
    /// Breaks begun, which is what decides when the next long one falls due.
    pub breaks_done: u32,
    /// How long the break in progress is, since that is not always `cfg.brk`.
    /// Zero when nothing is on screen.
    pub break_len: Duration,
}

#[derive(Debug, Clone)]
pub struct Scheduler {
    cfg: Config,
    state: State,
    warned: bool,
    deferred: bool,
    deferred_for: Duration,
    overdue_announced: bool,
    resume_overlay: bool,
    idle_credited: bool,
    postpones_used: u32,
    window_elapsed: Duration,
    released: bool,
    waiting: Duration,
    await_announced: bool,
    /// Breaks begun. Counted rather than derived from the clock so that the
    /// long one falls on every fourth *break*, not every fourth hour of a day
    /// you spent in meetings.
    breaks_done: u32,
    /// The length of the break in progress. Fixed when it starts rather than
    /// read from the config each tick: editing the config mid-break must not
    /// make the page on screen change its mind about how long it is.
    brk_len: Duration,
}

impl Scheduler {
    pub fn new(cfg: Config) -> Self {
        let cfg_brk = cfg.brk;
        Self {
            cfg,
            state: State::Working { worked: Duration::ZERO, due: false },
            warned: false,
            deferred: false,
            deferred_for: Duration::ZERO,
            overdue_announced: false,
            resume_overlay: false,
            idle_credited: false,
            postpones_used: 0,
            window_elapsed: Duration::ZERO,
            released: false,
            waiting: Duration::ZERO,
            await_announced: false,
            breaks_done: 0,
            brk_len: cfg_brk,
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn snapshot(&self) -> Snapshot {
        let (breaking, worked, rested, due) = match self.state {
            State::Working { worked, due } => (false, worked, Duration::ZERO, due),
            State::Breaking { rested } => (true, Duration::ZERO, rested, false),
        };
        Snapshot {
            breaking,
            worked,
            rested,
            due,
            postpones_used: self.postpones_used,
            window_elapsed: self.window_elapsed,
            released: self.released,
            waiting: self.waiting,
            breaks_done: self.breaks_done,
            break_len: match breaking {
                true => self.brk_len,
                false => Duration::ZERO,
            },
        }
    }

    /// Rebuild from a snapshot. Transient flags (warned, deferred) deliberately
    /// start clear: re-warning after a restart is harmless, silently swallowing
    /// a warning is not.
    pub fn restore(cfg: Config, snap: Snapshot) -> Self {
        let mut s = Self::new(cfg);
        s.state = if snap.breaking {
            State::Breaking { rested: snap.rested }
        } else {
            State::Working { worked: snap.worked, due: snap.due }
        };
        s.postpones_used = snap.postpones_used.min(s.cfg.postpone_budget);
        s.window_elapsed = snap.window_elapsed;
        // A scan already made is not made again: walking to the tag and back
        // only to have the service restart under you would be unforgivable.
        s.released = snap.breaking && snap.released;
        s.waiting = if snap.breaking { snap.waiting } else { Duration::ZERO };
        s.breaks_done = snap.breaks_done;
        // A break carries its own length across a restart. Without this a long
        // break resumed from the state file would come back as a short one and
        // end early, which is the sort of arithmetic nobody would ever notice
        // going wrong.
        s.brk_len = match snap.breaking && !snap.break_len.is_zero() {
            true => snap.break_len,
            false => s.cfg.brk,
        };
        // Restored in the middle of a break: nothing is on screen, so the
        // overlay has to be asked for again. Without this the rest of the break
        // counts down invisibly and enforces nothing.
        s.resume_overlay = snap.breaking;
        s
    }

    pub fn postpones_left(&self) -> u32 {
        self.cfg.postpone_budget.saturating_sub(self.postpones_used)
    }

    /// How long the break on screen runs for -- or, between breaks, how long
    /// the next one will. Not always `config().brk`: every `long_every`-th one
    /// is the long one.
    pub fn break_length(&self) -> Duration {
        match self.state {
            State::Breaking { .. } => self.brk_len,
            State::Working { .. } => self.length_of(self.breaks_done + 1),
        }
    }

    /// Whether the break on screen -- or the next one -- is a long one.
    pub fn long_break(&self) -> bool {
        self.break_length() != self.cfg.brk
    }

    /// How many more ordinary breaks before the long one. Zero while the long
    /// one is the next thing to happen; `None` when they are all the same.
    pub fn until_long(&self) -> Option<u32> {
        let every = self.cfg.long_every;
        if every == 0 || self.cfg.long_brk.is_zero() {
            return None;
        }
        Some((every - 1) - (self.breaks_done % every))
    }

    /// The length of the `nth` break, counting from one.
    fn length_of(&self, nth: u32) -> Duration {
        let every = self.cfg.long_every;
        match every > 0 && !self.cfg.long_brk.is_zero() && nth.is_multiple_of(every) {
            true => self.cfg.long_brk,
            false => self.cfg.brk,
        }
    }

    /// Advance by `delta`, given the system's current `idle` time and whether an
    /// inhibitor (screen share, presentation, call) is active.
    ///
    /// `delta` should come from a clock that includes suspend, so that closing
    /// the lid is credited as rest rather than silently skipped.
    pub fn tick(&mut self, delta: Duration, idle: Duration, inhibited: bool) -> Vec<Command> {
        let mut out = Vec::new();

        self.window_elapsed += delta;
        if self.window_elapsed >= self.cfg.postpone_window {
            self.window_elapsed = Duration::ZERO;
            self.postpones_used = 0;
        }

        match self.state {
            State::Working { mut worked, due } => {
                // Long enough away from the keyboard is a break, whatever the
                // reason. Ambushing someone the moment they sit back down is
                // how this class of tool gets uninstalled.
                if inhibited {
                    // A call, a video, a presentation. Something is holding the
                    // session awake because there is content on screen, so this
                    // is screen time whether or not your hands are moving --
                    // neither idle rule applies. Without this, sitting still
                    // through a meeting reads as rest and hands back a break
                    // you never took.
                    self.idle_credited = false;
                    worked += delta;
                } else if idle >= self.cfg.idle_credit {
                    // Credit an absence once, not once per tick. A real idle
                    // clock keeps climbing the whole time you are at lunch, so
                    // without this an hour away fires 3600 credit events.
                    if !self.idle_credited {
                        self.idle_credited = true;
                        self.reset_work();
                        out.push(Command::CreditedIdle { was_idle: idle });
                    }
                    return out;
                } else if idle < self.cfg.idle_pause {
                    // Away, but not long enough to count: hold the timer rather
                    // than banking work time that never happened. Back at the
                    // keyboard, the next absence earns its own credit.
                    self.idle_credited = false;
                    worked += delta;
                }

                let owed = due || worked >= self.cfg.work;

                if owed {
                    if inhibited {
                        self.state = State::Working { worked, due: true };
                        if !self.deferred {
                            self.deferred = true;
                            out.push(Command::Deferred);
                        }
                        self.deferred_for += delta;
                        let limit = self.cfg.defer_warn_after;
                        if !self.overdue_announced
                            && !limit.is_zero()
                            && self.deferred_for >= limit
                        {
                            self.overdue_announced = true;
                            out.push(Command::Overdue { waiting: self.deferred_for });
                        }
                    } else {
                        // Counted the moment the page goes up, so that the
                        // fourth break is long even if the third was cut short
                        // by a grace running out.
                        self.breaks_done += 1;
                        self.brk_len = self.length_of(self.breaks_done);
                        self.state = State::Breaking { rested: Duration::ZERO };
                        self.warned = false;
                        self.deferred = false;
                        out.push(Command::ShowOverlay { duration: self.brk_len });
                    }
                    return out;
                }

                self.state = State::Working { worked, due: false };
                let until = self.cfg.work.saturating_sub(worked);
                if !self.warned && !inhibited && until <= self.cfg.warn_before {
                    self.warned = true;
                    out.push(Command::Warn { until_break: until });
                }
            }
            State::Breaking { rested } => {
                // Time past the countdown is kept separately rather than piled
                // onto `rested`, so every "how much of the break is left" sum
                // below stays a subtraction that cannot go negative.
                let total = rested + delta;
                let rested = total.min(self.brk_len);
                self.state = State::Breaking { rested };

                if rested < self.brk_len {
                    let remaining = self.brk_len - rested;
                    if self.resume_overlay {
                        self.resume_overlay = false;
                        out.push(Command::ShowOverlay { duration: remaining });
                    }
                    out.push(Command::Tick { remaining });
                    return out;
                }

                // The time is served. Whether that is enough is the whole
                // question: with a release gate on, sitting out the countdown
                // at your desk was exactly the thing that needed fixing.
                if !self.cfg.require_release || self.released {
                    self.reset_work();
                    out.push(Command::HideOverlay);
                    return out;
                }

                self.waiting += total.saturating_sub(self.brk_len);
                // Restored mid-wait: the page has to come back before it can
                // say what it is waiting for. It carries the full break length
                // because the countdown it shows is already spent -- the
                // `AwaitRelease` that follows is what paints over it.
                if self.resume_overlay {
                    self.resume_overlay = false;
                    out.push(Command::ShowOverlay { duration: self.brk_len });
                }
                if !self.await_announced {
                    self.await_announced = true;
                    out.push(Command::AwaitRelease);
                }
                let grace = self.cfg.release_grace;
                if !grace.is_zero() && self.waiting >= grace {
                    let waited = self.waiting;
                    self.reset_work();
                    out.push(Command::GaveUpWaiting { waited });
                    out.push(Command::HideOverlay);
                }
            }
        }

        out
    }

    /// Buy `cfg.postpone` more work time, if any budget is left. Only valid
    /// while a break is imminent or owed — a budget you can spend early is a
    /// budget you spend all of.
    pub fn postpone(&mut self) -> PostponeResult {
        let State::Working { worked, due } = self.state else {
            return PostponeResult::NotPending;
        };
        if !due && !self.warned {
            return PostponeResult::NotPending;
        }
        if self.postpones_left() == 0 {
            return PostponeResult::Exhausted;
        }

        self.postpones_used += 1;
        let base = worked.min(self.cfg.work);
        self.state = State::Working {
            worked: base.saturating_sub(self.cfg.postpone),
            due: false,
        };
        self.warned = false;
        self.deferred = false;
        PostponeResult::Granted { remaining_budget: self.postpones_left() }
    }

    /// The release signal arrived — the tag was scanned.
    ///
    /// Deliberately not "end the break now": arriving early banks the signal
    /// and the countdown still has to run out. Otherwise the walk to the tag
    /// *replaces* the break instead of being the thing that proves you took
    /// one, and a five-minute rest becomes a ninety-second errand.
    pub fn released(&mut self) -> ReleaseResult {
        let State::Breaking { rested } = self.state else {
            return ReleaseResult::NotBreaking;
        };
        if !self.cfg.require_release {
            return ReleaseResult::NotRequired;
        }
        self.released = true;
        match self.brk_len.saturating_sub(rested) {
            left if left.is_zero() => ReleaseResult::Freed,
            left => ReleaseResult::Banked { remaining: left },
        }
    }

    /// Whether the break is sitting past its countdown waiting to be released.
    pub fn awaiting_release(&self) -> bool {
        self.await_announced && !self.released
    }

    /// End the current break early (debug/escape hatch; the overlay does not
    /// offer this).
    pub fn skip_break(&mut self) {
        if matches!(self.state, State::Breaking { .. }) {
            self.reset_work();
        }
    }

    fn reset_work(&mut self) {
        self.state = State::Working { worked: Duration::ZERO, due: false };
        self.warned = false;
        self.deferred = false;
        self.deferred_for = Duration::ZERO;
        self.overdue_announced = false;
        self.resume_overlay = false;
        self.released = false;
        self.waiting = Duration::ZERO;
        self.await_announced = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// Small, round numbers so the assertions stay readable.
    fn cfg() -> Config {
        Config {
            work: secs(100),
            brk: secs(20),
            warn_before: secs(10),
            idle_credit: secs(30),
            idle_pause: secs(10),
            postpone: secs(15),
            postpone_budget: 2,
            postpone_window: secs(1000),
            defer_warn_after: secs(50),
            require_release: false,
            release_grace: secs(30),
            long_every: 0,
            long_brk: secs(60),
        }
    }

    /// Run `n` one-second ticks, active and uninhibited, collecting commands.
    fn run(s: &mut Scheduler, n: u64) -> Vec<Command> {
        (0..n).flat_map(|_| s.tick(secs(1), Duration::ZERO, false)).collect()
    }

    #[test]
    fn warns_then_breaks_then_returns_to_work() {
        let mut s = Scheduler::new(cfg());

        let quiet = run(&mut s, 89);
        assert!(quiet.is_empty(), "no chatter before the warning window");

        let warned = run(&mut s, 1);
        assert_eq!(warned, vec![Command::Warn { until_break: secs(10) }]);

        // Warning fires once, not every tick.
        assert!(run(&mut s, 10).iter().all(|c| !matches!(c, Command::Warn { .. })));

        assert!(matches!(s.state(), State::Breaking { .. }));

        let ending = run(&mut s, 20);
        assert_eq!(ending.last(), Some(&Command::HideOverlay));
        assert_eq!(s.state(), State::Working { worked: Duration::ZERO, due: false });
    }

    #[test]
    fn every_fourth_break_is_the_long_one() {
        let long = Config { long_every: 4, long_brk: secs(60), ..cfg() };
        let mut s = Scheduler::new(long.clone());

        // Three of the ordinary length, and then one that is not.
        for nth in 1..=4 {
            assert_eq!(s.until_long(), Some(4 - nth));
            let out = run(&mut s, 100);
            let wanted = if nth == 4 { secs(60) } else { secs(20) };
            assert!(
                out.contains(&Command::ShowOverlay { duration: wanted }),
                "break {nth} should run for {wanted:?}: {out:?}"
            );
            assert_eq!(s.break_length(), wanted);
            assert_eq!(s.long_break(), nth == 4);
            // Sit the whole thing out, however long it is.
            let ending = run(&mut s, wanted.as_secs());
            assert_eq!(ending.last(), Some(&Command::HideOverlay), "break {nth} has to end");
        }
        // And round again: the fifth is short.
        assert_eq!(s.until_long(), Some(3));

        // Off by default, and off means every break is the same.
        let mut plain = Scheduler::new(cfg());
        assert_eq!(plain.until_long(), None);
        assert!(!plain.long_break());
        run(&mut plain, 100);
        assert_eq!(plain.break_length(), secs(20));
    }

    #[test]
    fn a_long_break_survives_a_restart_at_its_own_length() {
        // The arithmetic nobody would notice going wrong: a fifteen-minute
        // break resumed as a five-minute one just ends, early, silently.
        let long = Config { long_every: 2, long_brk: secs(60), ..cfg() };
        let mut s = Scheduler::new(long.clone());
        run(&mut s, 100);
        run(&mut s, 20);
        run(&mut s, 100);
        assert_eq!(s.break_length(), secs(60), "the second break is the long one");

        let half_way = run(&mut s, 30);
        assert!(half_way.iter().any(|c| matches!(c, Command::Tick { .. })));
        let mut back = Scheduler::restore(long, s.snapshot());
        assert_eq!(back.break_length(), secs(60));
        // Thirty seconds served, so thirty to go and not ten.
        let ending = run(&mut back, 29);
        assert!(!ending.contains(&Command::HideOverlay), "ended early: {ending:?}");
        assert_eq!(run(&mut back, 1).last(), Some(&Command::HideOverlay));
    }

    #[test]
    fn long_idle_counts_as_the_break() {
        let mut s = Scheduler::new(cfg());
        run(&mut s, 90);

        let out = s.tick(secs(1), secs(30), false);
        assert_eq!(out, vec![Command::CreditedIdle { was_idle: secs(30) }]);
        assert_eq!(s.state(), State::Working { worked: Duration::ZERO, due: false });
    }

    #[test]
    fn a_long_absence_is_credited_once_not_every_second() {
        let mut s = Scheduler::new(cfg());
        run(&mut s, 90);

        // Ten minutes away, with a real idle clock that keeps climbing.
        let mut credits = 0;
        for i in 0..600 {
            for c in s.tick(secs(1), secs(30 + i), false) {
                if matches!(c, Command::CreditedIdle { .. }) {
                    credits += 1;
                }
            }
        }
        assert_eq!(credits, 1, "one absence, one credit");
    }

    #[test]
    fn a_second_absence_earns_its_own_credit() {
        let mut s = Scheduler::new(cfg());
        run(&mut s, 90);
        s.tick(secs(1), secs(30), false);

        run(&mut s, 5); // back at the keyboard

        let out = s.tick(secs(1), secs(30), false);
        assert_eq!(out, vec![Command::CreditedIdle { was_idle: secs(30) }]);
    }

    #[test]
    fn short_idle_pauses_the_work_timer() {
        let mut s = Scheduler::new(cfg());
        run(&mut s, 50);

        // Away 15s: past idle_pause, short of idle_credit. Banks nothing.
        for _ in 0..15 {
            s.tick(secs(1), secs(15), false);
        }
        assert_eq!(s.state(), State::Working { worked: secs(50), due: false });
    }

    #[test]
    fn a_call_counts_as_work_however_still_you_sit() {
        let mut s = Scheduler::new(cfg());

        // 60s on a call: idle far past idle_credit, but inhibited throughout.
        for _ in 0..60 {
            let out = s.tick(secs(1), secs(600), true);
            assert!(
                !out.iter().any(|c| matches!(c, Command::CreditedIdle { .. })),
                "sitting still on a call is not a break"
            );
        }
        assert_eq!(s.state(), State::Working { worked: secs(60), due: false });
    }

    #[test]
    fn a_stuck_inhibitor_is_reported_but_never_interrupts() {
        let mut s = Scheduler::new(cfg());
        run(&mut s, 99);

        // The break falls due, then an app holds it for a long time.
        let mut overdue = Vec::new();
        for _ in 0..200 {
            for c in s.tick(secs(1), Duration::ZERO, true) {
                match c {
                    Command::Overdue { waiting } => overdue.push(waiting),
                    Command::ShowOverlay { .. } => panic!("must never interrupt a call"),
                    _ => {}
                }
            }
        }
        assert_eq!(overdue, vec![secs(50)], "announced once, at the configured limit");

        // Call ends: the break lands immediately.
        let out = s.tick(secs(1), Duration::ZERO, false);
        assert_eq!(out, vec![Command::ShowOverlay { duration: secs(20) }]);
    }

    #[test]
    fn inhibitor_defers_the_break_without_losing_it() {
        let mut s = Scheduler::new(cfg());
        run(&mut s, 99);

        let out = s.tick(secs(1), Duration::ZERO, true);
        assert_eq!(out, vec![Command::Deferred]);

        // Deferral announces itself once, then waits quietly.
        assert!(s.tick(secs(1), Duration::ZERO, true).is_empty());
        assert!(matches!(s.state(), State::Working { due: true, .. }));

        // Screen share ends; the owed break fires immediately.
        let out = s.tick(secs(1), Duration::ZERO, false);
        assert_eq!(out, vec![Command::ShowOverlay { duration: secs(20) }]);
    }

    #[test]
    fn a_snapshot_round_trips() {
        let mut s = Scheduler::new(cfg());
        run(&mut s, 95);
        s.postpone();
        run(&mut s, 20);

        let restored = Scheduler::restore(cfg(), s.snapshot());
        assert_eq!(restored.snapshot(), s.snapshot());
        assert_eq!(restored.postpones_left(), s.postpones_left());
    }

    #[test]
    fn a_restart_mid_break_resumes_the_break() {
        let mut s = Scheduler::new(cfg());
        run(&mut s, 100);
        run(&mut s, 5); // 5s into a 20s break

        let mut restored = Scheduler::restore(cfg(), s.snapshot());
        assert!(matches!(restored.state(), State::Breaking { .. }));

        // The remaining 15s still has to be served -- behind an overlay that
        // the restore puts back on the first tick.
        assert!(
            run(&mut restored, 14)
                .iter()
                .all(|c| matches!(c, Command::Tick { .. } | Command::ShowOverlay { .. }))
        );
        assert_eq!(restored.tick(secs(1), Duration::ZERO, false), vec![Command::HideOverlay]);
    }

    #[test]
    fn a_restart_mid_break_puts_the_overlay_back() {
        let mut s = Scheduler::new(cfg());
        run(&mut s, 100);
        run(&mut s, 5); // 5s into a 20s break

        let mut restored = Scheduler::restore(cfg(), s.snapshot());
        let out = restored.tick(secs(1), Duration::ZERO, false);

        // Without this the break counts down behind nothing at all: the
        // scheduler believes it is enforcing a break while the screen is free.
        assert_eq!(
            out.first(),
            Some(&Command::ShowOverlay { duration: secs(14) }),
            "the overlay must come back, showing what is left"
        );
        // Asked for once, not on every tick that follows.
        assert!(
            !restored
                .tick(secs(1), Duration::ZERO, false)
                .iter()
                .any(|c| matches!(c, Command::ShowOverlay { .. }))
        );
    }

    #[test]
    fn a_restart_after_the_break_ended_does_not_flash_an_overlay() {
        let mut s = Scheduler::new(cfg());
        run(&mut s, 100);
        run(&mut s, 5);

        // Gone long enough that the break finished while nothing was running.
        let mut restored = Scheduler::restore(cfg(), s.snapshot());
        let out = restored.tick(secs(60), Duration::ZERO, false);
        assert_eq!(out, vec![Command::HideOverlay]);
    }

    #[test]
    fn a_shrunk_budget_cannot_be_exceeded_by_an_old_snapshot() {
        let mut s = Scheduler::new(cfg());
        run(&mut s, 95);
        s.postpone();

        let mut tighter = cfg();
        tighter.postpone_budget = 0;
        let restored = Scheduler::restore(tighter, s.snapshot());
        assert_eq!(restored.postpones_left(), 0);
    }

    #[test]
    fn postpone_is_budgeted_and_only_when_pending() {
        let mut s = Scheduler::new(cfg());

        run(&mut s, 10);
        assert_eq!(s.postpone(), PostponeResult::NotPending, "cannot bank postpones early");

        run(&mut s, 80); // 90s worked: inside the warning window
        assert_eq!(s.postpone(), PostponeResult::Granted { remaining_budget: 1 });
        assert_eq!(s.state(), State::Working { worked: secs(75), due: false });

        run(&mut s, 15);
        assert_eq!(s.postpone(), PostponeResult::Granted { remaining_budget: 0 });

        run(&mut s, 15);
        assert_eq!(s.postpone(), PostponeResult::Exhausted);

        // Budget gone: the break lands.
        let out = run(&mut s, 10);
        assert!(out.contains(&Command::ShowOverlay { duration: secs(20) }));
    }

    #[test]
    fn postpone_budget_refills_each_window() {
        let mut cfg = cfg();
        cfg.postpone_window = secs(200);
        let mut s = Scheduler::new(cfg);

        run(&mut s, 95);
        assert_eq!(s.postpone(), PostponeResult::Granted { remaining_budget: 1 });
        run(&mut s, 10); // back into the warning window
        assert_eq!(s.postpone(), PostponeResult::Granted { remaining_budget: 0 });

        // Idle through the rest of the window so no break intervenes.
        for _ in 0..200 {
            s.tick(secs(1), secs(30), false);
        }
        assert_eq!(s.postpones_left(), 2);
    }

    #[test]
    fn suspend_gap_is_credited_as_rest() {
        let mut s = Scheduler::new(cfg());
        run(&mut s, 90);

        // Lid closed for an hour: one huge delta, idle just as large.
        let out = s.tick(secs(3600), secs(3600), false);
        assert_eq!(out, vec![Command::CreditedIdle { was_idle: secs(3600) }]);
    }

    /// A break you have to be released from, with a 30s grace on top.
    fn gated() -> Config {
        Config { require_release: true, release_grace: secs(30), ..cfg() }
    }

    #[test]
    fn a_gated_break_does_not_end_when_the_countdown_does() {
        let mut s = Scheduler::new(gated());
        run(&mut s, 100); // break starts

        let out = run(&mut s, 20); // the whole 20s of it
        assert_eq!(
            out.iter().filter(|c| matches!(c, Command::AwaitRelease)).count(),
            1,
            "says once that it is waiting, then waits quietly"
        );
        assert!(
            !out.contains(&Command::HideOverlay),
            "sitting out the countdown must not hand the desk back"
        );
        assert!(matches!(s.state(), State::Breaking { .. }));
        assert!(s.awaiting_release());
    }

    #[test]
    fn a_scan_part_way_through_is_banked_and_the_break_still_runs() {
        let mut s = Scheduler::new(gated());
        run(&mut s, 100);
        run(&mut s, 5); // 5s into a 20s break: up, and away from the desk

        assert_eq!(s.released(), ReleaseResult::Banked { remaining: secs(15) });
        assert!(!s.awaiting_release(), "nothing is being waited for");

        // The rest of the break is served as normal -- and then it just ends.
        // Walking to the tag is not a way of buying the other 15 seconds back.
        let out = run(&mut s, 15);
        assert_eq!(out.last(), Some(&Command::HideOverlay));
        assert!(!out.contains(&Command::AwaitRelease), "nothing left to ask for");
        assert_eq!(s.state(), State::Working { worked: Duration::ZERO, due: false });
    }

    #[test]
    fn a_scan_after_the_countdown_gives_the_desk_back_at_once() {
        let mut s = Scheduler::new(gated());
        run(&mut s, 100);
        run(&mut s, 25); // 5s past the end, waiting

        assert_eq!(s.released(), ReleaseResult::Freed);
        assert_eq!(s.tick(secs(1), Duration::ZERO, false), vec![Command::HideOverlay]);
    }

    #[test]
    fn a_release_nobody_asked_for_changes_nothing() {
        let mut s = Scheduler::new(cfg()); // ungated
        assert_eq!(s.released(), ReleaseResult::NotBreaking);
        run(&mut s, 100);
        assert_eq!(s.released(), ReleaseResult::NotRequired);
        // ...and the break still ends on its own, exactly as before.
        assert_eq!(run(&mut s, 20).last(), Some(&Command::HideOverlay));
    }

    #[test]
    fn a_signal_that_never_comes_gives_up_after_the_grace() {
        let mut s = Scheduler::new(gated());
        run(&mut s, 100);
        run(&mut s, 20); // countdown done

        let out = run(&mut s, 30); // the full grace, with no scan
        assert_eq!(out[out.len() - 2], Command::GaveUpWaiting { waited: secs(30) });
        assert_eq!(out.last(), Some(&Command::HideOverlay));
        assert_eq!(s.state(), State::Working { worked: Duration::ZERO, due: false });
    }

    #[test]
    fn a_grace_of_zero_waits_for_as_long_as_it_takes() {
        let mut s = Scheduler::new(Config { release_grace: Duration::ZERO, ..gated() });
        run(&mut s, 100);

        let out = run(&mut s, 3600); // an hour past the end of a 20s break
        assert!(!out.contains(&Command::HideOverlay));
        assert!(s.awaiting_release());

        assert_eq!(s.released(), ReleaseResult::Freed);
        assert_eq!(s.tick(secs(1), Duration::ZERO, false), vec![Command::HideOverlay]);
    }

    #[test]
    fn a_restart_while_waiting_puts_the_page_back_and_asks_again() {
        let mut s = Scheduler::new(gated());
        run(&mut s, 100);
        run(&mut s, 25); // waiting, 5s in

        let mut restored = Scheduler::restore(gated(), s.snapshot());
        let out = restored.tick(secs(1), Duration::ZERO, false);

        // Without the overlay coming back, a restart mid-wait would leave the
        // screen free while the scheduler still believed it was holding it.
        assert_eq!(out[0], Command::ShowOverlay { duration: secs(20) });
        assert_eq!(out[1], Command::AwaitRelease);

        // The grace picks up where it left off rather than restarting.
        let out = run(&mut restored, 24);
        assert!(out.contains(&Command::HideOverlay), "30s of grace, 6s of it before the restart");
    }

    #[test]
    fn a_scan_survives_a_restart() {
        let mut s = Scheduler::new(gated());
        run(&mut s, 100);
        run(&mut s, 5);
        s.released();

        // Walking to the tag and back only to be asked to do it again is the
        // one failure this must not have.
        let mut restored = Scheduler::restore(gated(), s.snapshot());
        let out = run(&mut restored, 15);
        assert!(!out.contains(&Command::AwaitRelease));
        assert_eq!(out.last(), Some(&Command::HideOverlay));
    }

    #[test]
    fn break_is_not_shortened_by_activity() {
        let mut s = Scheduler::new(cfg());
        run(&mut s, 100);
        assert!(matches!(s.state(), State::Breaking { .. }));

        // Typing through the overlay must not end it early.
        for _ in 0..19 {
            let out = s.tick(secs(1), Duration::ZERO, false);
            assert!(matches!(out.as_slice(), [Command::Tick { .. }]));
        }
        assert_eq!(s.tick(secs(1), Duration::ZERO, false), vec![Command::HideOverlay]);
    }
}

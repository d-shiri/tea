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
}

/// What a postpone would buy, and how many are left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snooze {
    pub duration: Duration,
    pub left: u32,
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
}

impl Scheduler {
    pub fn new(cfg: Config) -> Self {
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
        // Restored in the middle of a break: nothing is on screen, so the
        // overlay has to be asked for again. Without this the rest of the break
        // counts down invisibly and enforces nothing.
        s.resume_overlay = snap.breaking;
        s
    }

    pub fn postpones_left(&self) -> u32 {
        self.cfg.postpone_budget.saturating_sub(self.postpones_used)
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
                        self.state = State::Breaking { rested: Duration::ZERO };
                        self.warned = false;
                        self.deferred = false;
                        out.push(Command::ShowOverlay { duration: self.cfg.brk });
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
            State::Breaking { mut rested } => {
                rested += delta;
                if rested >= self.cfg.brk {
                    self.reset_work();
                    out.push(Command::HideOverlay);
                } else {
                    self.state = State::Breaking { rested };
                    let remaining = self.cfg.brk - rested;
                    if self.resume_overlay {
                        self.resume_overlay = false;
                        out.push(Command::ShowOverlay { duration: remaining });
                    }
                    out.push(Command::Tick { remaining });
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

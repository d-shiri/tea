# tea

Stops you working every N minutes and makes you rest for M.

Target: Ubuntu GNOME / Wayland. Soft enforcement — a fullscreen overlay that
re-raises when you switch away. Dodgeable if you're determined; annoying enough
to work.

## Config

Created on first run at `$XDG_CONFIG_HOME/tea/config.toml`, else
`~/.config/tea/config.toml`. `--config <path>` points elsewhere,
`--write-config` writes the starter file and exits, and every setting has a
matching flag that overrides the file (`tea --help`).

    work = "25m"
    break = "5m"
    warn_before = "30s"

    [idle]
    credit = "5m"
    pause = "1m"

    [postpone]
    duration = "3m"
    budget = 2
    window = "1h"

Durations are `"90s"`, `"25m"`, `"1h"`; a bare number means minutes. Unknown
keys are a hard error — a silently ignored typo in a config you edit twice a
year is worse than a crash on startup.

## Status

    $ tea status

      tea — working

      work      ██████████████░░░░░░░░░░   60%
                15m of 25m — next break in 10m

      postpone  1 of 2 left — resets in 40m
      now       idle 4s · nothing is holding a break
      service   running · saved 2s ago
      config    ~/.config/tea/config.toml

Read from the state file the service already writes, so it is accurate to
within one save interval. It shows the config file's values — if you edit the
config while the service is running, restart it before trusting the numbers.

## Layout

    crates/core   pure state machine + the `Blocker` trait — no clock, no I/O
    crates/cli    host: config, clock, D-Bus session, GTK overlay, terminal
    dist/         systemd user unit + install script

GTK owns the main loop; the scheduler rides a 1s `glib` timeout on it. No
threads and no locking anywhere in the program.

The scheduling rules live entirely in `core` and are exercised without waiting
five minutes for anything. Everything platform-shaped stays outside it.

## Rules that matter more than the timer

- **Idle credit** — away 5+ minutes? That was your break. A tool that ambushes
  you the moment you sit back down gets uninstalled in two days. Idle comes from
  `org.gnome.Mutter.IdleMonitor`, and an absence is credited *once*, not once
  per second.
- **Idle pause** — away 1–5 minutes banks no work time.
- **Break debt survives a restart** — state in `$XDG_STATE_HOME/tea/state.toml`.
  Downtime while the machine was *up* counts as work, so restarting the service
  is not a dodge; downtime across a *reboot* counts as rest. Reboot is detected
  by boottime going backwards, which is the one thing a reboot cannot fake.
- **Suspend counts** — timing runs on CLOCK_BOOTTIME (`/proc/uptime`), not
  `Instant`, so a closed lid is rest rather than time that never happened.
- **Calls count as work** — while an app holds the session awake (a call, a
  video, a presentation) you are looking at a screen, so neither idle rule
  applies. Without this, sitting still through an hour of meetings reads as an
  hour of rest and hands back breaks you never took.
- **A held-up break is never forced through** — covering the screen mid-call is
  the one thing this must not do. After `calls.warn_after` it says who is
  holding it (`tea --probe` names them too) and leaves it at that.
- **Inhibitors** — a break owed during a screen share is deferred, not lost.
  Read from `org.gnome.SessionManager.IsInhibited(8)`, the same flag video
  players and presentation mode set.
- **Degrades, never fails** — no session bus (SSH, gnome-shell restart) costs
  accuracy, not availability: it falls back to suspend-gap detection and says so
  once.
- **Postpone is budgeted** — 2 per hour, only once a break is imminent.
  Unlimited snooze is the same as no tool.

## Roadmap

- [x] **P0** state machine + CLI host
- [x] **P1** TOML config, GTK4 overlay, `systemd --user` unit
- [x] **P2a** real idle via `org.gnome.Mutter.IdleMonitor` + inhibitor detection
- [x] **P2b** break debt persisted to `$XDG_STATE_HOME`
- [ ] **P3** *only if you keep dodging it* — GNOME Shell extension for a real
      input grab

`P1` puts the overlay behind a `trait Blocker`, so `P3` (or a layer-shell
backend for Sway/KDE) is an addition rather than a rewrite.

## Install

    ./dist/install.sh    # builds, installs to /usr/local/bin, enables the user service

Needs `libgtk-4-dev` to build.

It runs as a **user** service, not a system one — `sudo systemctl` looks at the
wrong manager and reports the unit as missing:

    systemctl --user status tea
    journalctl --user -u tea -f
    systemctl --user restart tea     # required after reinstalling the binary

## Try it

    tea run                    # show the break page, for as long as a real break
    tea run 5s                 # ...or for however long you say
    tea run-warning            # show the warning toast
    tea reload                 # pick up edited settings
    tea status                 # what the running service is doing
    tea config                 # every setting, nicely laid out
    tea set-work 30m           # change a setting, comments preserved
    tea set-break 5m
    tea set-warn 30s
    tea --probe                # print live idle/inhibitor readings and exit
    tea --headless             # terminal only; the only thing that works over SSH
    tea --work 5s --break 3s --postpone-budget 0

Only one instance runs at a time — a second `tea` hands off to the first
rather than starting a competing timer, and the first ignores the handoff
rather than starting a second timer inside itself.

Breaks are silent by default — point `tea set-sound <file>` at a sound you
actually like, or set `[sound] mode = "voice"` to have it spoken. The overlay
fades in rather than appearing all at once. The 30-second
warning only opens a window when there is a postpone to click; otherwise it is a
desktop notification, because a window you can do nothing about is just noise.

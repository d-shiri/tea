# tea

![The break page: a countdown ring that drains as the break runs](assets/break-page.png)

You mean to take breaks. You don't.

tea waits in the background while you work, and every so often it takes the
screen for a few minutes. There's a countdown. When it runs out you get your
desk back.

It tries not to be daft about it:

- If you've already been away from the keyboard, that counted as your break —
  it won't ambush you the second you sit back down.
- If you're in a call or watching something, it waits until you're done rather
  than dropping a black screen over your face.
- If you genuinely can't stop right now, there's a button for that. Twice an
  hour, so it stays a reprieve and not a habit.

By default it can't physically hold you there — you can still switch away if
you're determined. The aim is for stopping to be easier than dodging. If that
is too easy, `hold.mode = "insist"` makes the page put itself back in front
every time you leave it, and `nfc.mode = "on"` stops the page lifting on the
countdown alone: it lifts when a tag in another room says you actually went.

Built for Ubuntu GNOME on Wayland.

    ./dist/install.sh     # build it, install it, start it running
    tea status            # see what it is up to
    tea set-work 30m      # change your mind

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
    duration = "1m"
    budget = 2
    window = "1h"

    [hold]
    mode = "soft"
    recheck = "400ms"

    [nfc]
    mode = "off"
    listen = "127.0.0.1:9797"
    url = ""
    token = ""
    grace = "10m"
    prompt = "Scan the tag to get your desk back"

    [nfc.home_assistant]
    url = ""
    token = ""
    token_file = ""
    entity = ""
    poll = "2s"

Durations are `"90s"`, `"25m"`, `"1h"`; a bare number means minutes. Unknown
keys are a hard error — a silently ignored typo in a config you edit twice a
year is worse than a crash on startup.

## Holding the screen

`hold.mode` decides what the break page does when you switch away from it.

    soft      it covers the screen and leaves it at that (default)
    insist    it puts itself back in front, for the whole break

Wayland is the reason there is a choice at all. There is no keyboard grab and
Mutter implements no layer-shell, so no client can own the screen; `insist` is
attrition instead. Every `recheck` it asks whether you are working *beside* the
break: input arriving while none of the pages holds the focus. Input is the
tell, not focus alone — GNOME refuses focus to windows you never touched, so a
page can be covering every screen, doing its job, without being "active", and
fighting over that would be a strobe. Keep your hands off the keyboard and
nothing on screen so much as blinks.

When you *are* still typing or mousing, the page asks for the focus back. GNOME
turns that request down when it comes from a window you didn't just touch, so
after a few refusals the page is *built again* — a brand new window, carrying
the time that's actually left, put up before the old one is destroyed behind
it. A window that has just appeared is one the compositor will raise.

Built again, not hidden and re-shown. Re-showing is the obvious way to look new
and it works for about a second: the surface comes back mapped and the right
size, but its frame clock never resumes, so nothing is drawn into it ever again.
That gives you a page that is unmistakably there and completely black — the
worst of both, since it covers the screen without telling you how long is left.

Every screen comes back, not just the one you left from. Only one window can
hold the focus, but any of the others can be *buried* — raise something on your
second monitor and a page that only guarded the first would leave you a desk to
work at. So the question asked each `recheck` is about all the screens at once:
if none of the pages is the active window, all of them are put back. Clicking
the page on the second screen is not an escape, and does not make the first one
snatch the focus away.

Screens that come and go mid-break are followed, in both modes: dock the laptop
and the new monitors get pages carrying the time that's left, undock it and the
survivors keep theirs.

None of it stops someone who keeps switching away, and nothing here should.
`insist` makes leaving a thing you have to keep choosing.

## The tag on the wall

The break page is scrupulous about time and completely blind to you. You can
sit through all five minutes of it without moving, and it will never know.
Waiting out a countdown at your own desk is not a break; it is a pause in the
typing.

`nfc.mode = "on"` makes the page wait for evidence instead. Put an NFC tag
somewhere you have to stand up and walk to — the kitchen, the hall, the other
end of the flat. The page now lifts when two things are true: the time has run
out, **and** the tag has been scanned. `tea set-nfc on` switches that on.

How the scan gets from the tag to here is a separate question, with two answers:
tea can **ask** something that already knows, or it can **listen** for something
to tell it. Asking is better in every way that matters, so it comes first.

**Scanning early counts.** Go in the first minute if you like: the scan is
banked, the countdown still runs its full length, and then the page just goes
away without asking anything else of you. The obvious alternative — the tag
ending the break on the spot — turns a five-minute rest into a ninety-second
errand, which is the same dodge in better shoes.

While the break runs, the page says where it stands: a contactless mark and
*Tag not scanned yet* at the foot of it, which turns into a green tick and *Tag
scanned* the moment the scan lands — on every screen, and on any page rebuilt
after it. Without that the laptop says nothing back and the natural thing to
assume is that it did not work, so you walk down the hall and scan it again.
Both glyphs are drawn rather than taken from the icon theme, for the same
reason the mark on the page is embedded: an icon that is missing on someone
else's machine is a blank square in the middle of the one screen they cannot
dismiss.

**Nothing is held hostage.** If the countdown ends with no scan, the ring stops
being drawn — there is nothing left to count — and the page says what it wants
instead, in the middle where the clock was (`prompt`: yours knows where your tag
is).
After `grace` it gives up, says so, and hands the desk back anyway: a flat
phone or a rebooting router must not cost you an afternoon. `grace = "off"`
for anyone who wants it strict. `tea unlock` does the same thing from a
terminal — the escape hatch, and obviously the cheat. It stays a command you
have to type, which is about as much friction as an escape hatch deserves.

A break that is interrupted by a restart is not restarted: the scan, and how
long the page has been waiting, are both in the state file with everything else.
Walking to the tag and being asked to do it again is the one failure this
could not have.

### Ask, don't listen

Home Assistant already knows. Its companion app fires `tag_scanned` the moment
a tag is read, and the tag turns up as an entity — so the whole problem inverts.
tea asks, on a call it makes itself, and nothing has to reach this machine at
all:

    [nfc.home_assistant]
    url = "http://homeassistant.local:8123"
    token_file = "~/.config/tea/.env"    # TEA_HA_TOKEN=…, chmod 600
    entity = "tag.living_room"
    poll = "2s"

The token is a long-lived access token, from the bottom of your Home Assistant
profile page. It can go straight in the config as `token = "…"`, but a config
file is a thing people paste into chat windows, so `token_file` points at
something else instead — `.env` shaped (`TEA_HA_TOKEN=…`, `#` comments, an
optional `export`), or just the token on a line by itself. `$TEA_HA_TOKEN` is
read if neither is set. tea says so, once, if it finds that file readable by
anyone else.

Any change of that entity's state is read as a scan, so it does not have to be a
tag entity — or a tag. An `input_button` pressed by an automation on the
`tag_scanned` trigger does the same job in two lines of YAML, which is also the
fallback if your Home Assistant is too old to make tag entities. A Zigbee button
by the kettle would work just as well.

    tea --probe

asks once and prints what came back, which is where a mistyped token or entity
name gets caught — rather than at the end of a break, in another room, with a
phone in your hand.

Three things fall out of asking rather than listening:

- **Nothing listens here.** No port, no firewall hole, no tunnel, no proxy, and
  no token of tea's own to end up on a sticker.
- **Nothing is asked of the hub except during a break.** The page goes up, the
  polling starts; the page comes down, it stops. The other twenty-five minutes
  are silent.
- **tea can tell whether the tag could be seen at all**, because it is the one
  making the call. If the hub cannot be reached, the page says so instead of
  asking for a walk that nothing would notice, and the break ends on its
  countdown rather than making you sit out the grace waiting for a scan that was
  never going to arrive. A gate nobody can open is not a gate, it is a lock.

Two rules that are less obvious. The first look of each break is a *baseline*,
never a scan: whatever the entity says when the page appears is what "not yet"
looks like here. Otherwise a tag touched on the way past at three o'clock would
quietly pay for the four o'clock break. And a hub that restarts hands out
`unknown` again — a change, and emphatically not somebody walking to the hall —
so those states are never read as one.

### The ear

If nothing on your network already knows about the tag, tea can listen instead,
and the tag's URL points at tea itself. Everything from here down is about that
arrangement; with Home Assistant doing the watching, none of it applies.

The thing listening is about as small as an HTTP server gets: one endpoint,
`/unlock`, plus `/status` for anything that wants to ask. It answers a browser
with a page and everything else with a line, because a phone that just scanned
a tag is holding a browser and wants to see that it worked.

    curl "http://…/unlock?token=…"              # the tag's own URL
    curl -H 'X-Tea-Token: …' -X POST http://…/unlock   # or a hub, or a shortcut

It runs on the same main loop as everything else — `gio`'s socket service, so
still no threads and nothing to lock — and it never touches the scheduler. A
request can land in the middle of a tick, so the two speak through a letterbox:
the tick leaves the state, the server leaves the scan, the same way the postpone
button has always worked.

### Two ways in

`listen` defaults to loopback, which answers this machine and nothing else. A
phone in another room needs one of two things.

**Bind it to the network.** `listen = "0.0.0.0:9797"`, plus — on Ubuntu, where
the firewall drops it before tea ever sees it — a hole opened only to the
network the tag is on:

    sudo ufw allow from 192.168.1.0/24 to any port 9797 proto tcp comment tea

Worth being clear-eyed about: that is a port listening on whatever network you
happen to be on, including the one at the café, speaking plain HTTP with the
token in the URL. The token's whole authority is *ending a break early*, which
is worth about what it sounds like — but let `tea set-nfc on` generate it rather
than typing one you would recognise.

**Or put something in front of it.** Anything that can reverse-proxy — on this
machine or reachable from it — can take the tag's request and pass it on, which
buys a real hostname and TLS over the token. tea has no idea that happened, so
tell it where its front door is, or the URL it prints for the tag will be the
one nothing can reach:

    [nfc]
    listen = "127.0.0.1:9797"
    url = "https://tea.example.home/unlock"

Scans then arrive from the proxy rather than the phone, so the log names
whoever the proxy says it was carrying — `X-Forwarded-For`, for the log and
nothing else. A header is a claim, never an authorisation.

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
    crates/cli    host: config, clock, D-Bus session, GTK overlay, terminal,
                  and the two ends of the tag: a Home Assistant poll, and a
                  small HTTP ear for anything that would rather push
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
- **A scan is proof of going, not a way out** — with the tag on, an early scan
  never shortens a break and a late one never lengthens it. All it decides is
  whether the page lifts when the countdown does.

## Roadmap

- [x] **P0** state machine + CLI host
- [x] **P1** TOML config, GTK4 overlay, `systemd --user` unit
- [x] **P2a** real idle via `org.gnome.Mutter.IdleMonitor` + inhibitor detection
- [x] **P2b** break debt persisted to `$XDG_STATE_HOME`
- [x] **P2c** the tag on the wall — a break the countdown alone cannot end
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
    tea set-nfc on             # hold the page until the tag is scanned
    tea unlock                 # ...scan it from here, without the tag
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

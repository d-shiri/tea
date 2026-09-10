# tea

![The break page: a countdown ring that drains as the break runs, with the tag and the walk still owed at the foot of it](assets/break-page.png)

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
- If you genuinely can't stop *this afternoon*, `tea off 2h` is the honest
  version of that, and `[hours]` is the standing one: outside your working
  hours nothing is counted and nothing appears.

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

    [hours]
    from = "off"
    to = "off"
    days = "all"

    [long]
    every = 0
    length = "15m"

    [hold]
    mode = "soft"
    recheck = "400ms"

    [page]
    accent = "#7aa2ff"
    background = "dark"
    font = ""
    prompts = "off"
    prompt_every = "20s"

    [port]
    listen = "127.0.0.1:9797"
    token = ""

    [nfc]
    mode = "off"
    url = ""
    grace = "10m"
    prompt = "Scan the tag to get your desk back"

    [nfc.home_assistant]
    url = ""
    token = ""
    token_file = ""
    entity = ""
    poll = "2s"
    nudge = ""
    publish = "off"
    publish_entity = "sensor.tea"

    [nfc.steps]
    mode = "off"
    count = 20
    entity = ""
    sync = "batched"

    [nfc.moving]
    mode = "off"
    entity = ""
    for = "auto"
    states = ["walking", "on_foot", "running"]

    [nfc.chores]
    mode = "off"
    entity = ""
    title = ""
    show = 8

Durations are `"90s"`, `"25m"`, `"1h"`; a bare number means minutes. Unknown
keys are a hard error — a silently ignored typo in a config you edit twice a
year is worse than a crash on startup.

## When tea is awake

Everything else here is about how long you have been working. This is the one
setting that cares what time it is.

    [hours]
    from = "09:00"
    to = "18:00"
    days = "mon-fri"

Outside those, tea is asleep: no warning, no page, and the clock stops. Not
paused — *stopped*. An evening film is not a work session with the timer held,
and coming back on Monday to a break that fell due on Saturday afternoon is
exactly the ambush the idle rules exist to prevent.

Either end can be `"off"` for no limit there; setting only `from` means "not
before nine" and runs to midnight. A window that reads backwards wraps it —
`from = "22:00"`, `to = "06:00"` is one night shift, not an empty set. Days
take names, lists or ranges, and ranges may wrap: `"fri-mon"` is a long
weekend.

The same idea for one afternoon:

    tea off 2h      # nothing until then. Bare `tea off` is an hour.
    tea on          # back, now

No restart and no reload: the running service reads the switch on its next
tick, and a break already on screen comes down with it — "leave me alone"
that starts with five minutes of not leaving you alone is a joke. Time spent
off is neither work banked nor rest credited. It simply did not happen.

## A longer one, every so often

    [long]
    every = 4
    length = "15m"

Four five-minute breaks in a row are four chances to stand up and no chance to
go anywhere. Every fourth one runs for fifteen instead: the walk, the coffee,
the thing that does not fit in three hundred seconds. `every = 0` turns it off,
and a `length` shorter than `break` is read as the typo it is and ignored.

Counted in breaks rather than in hours, so a morning spent in meetings does not
quietly spend your long one. The break on screen carries its own length in the
state file, so a restart mid-way through a long break resumes a long break.

## Holding the screen

`hold.mode` decides what the break page does when you switch away from it.

    soft      it covers the screen and leaves it at that (default)
    insist    it puts itself back in front, for the whole break
    strict    insist, with the desktop's ways out switched off

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

`strict` is for the person who has caught themselves doing exactly that. On
GNOME every shortcut is a dconf setting, and a process in the session can
change them and see them take effect at once — so while a strict page is up,
the Super key, the overview and app grid, Alt-Tab and its cousins, the
workspace switches, the dock's Super+1…9 and the hot corner are all set to
nothing, and when the page comes down they are put back exactly as they were.
Ctrl-Alt-F3 and the power button are below the desktop and stay; leaving takes
a decision rather than a reflex, which is the point. The lock screen shortcut
is never touched.

What was there is written to `held-keys.json` beside the state file *before*
anything is changed, and that file is read back and honoured at every start of
tea — daemon or `tea run` — so a daemon killed mid-break puts the keyboard
right the moment it is next run. `tea off` mid-break puts it back too, since
it takes the page down.
`insist` makes leaving a thing you have to keep choosing.

### The one way out

Everything above is about making a break hard to walk away from. This is the
one thing on the page that ends it on purpose: **hold H** — for help — for two
seconds, and the break is cancelled.

One a day. Spend it and the page says so the next time you press, and there is
no second one until tomorrow; the count rolls over at midnight with the rest of
the day's numbers. There is no setting for it, deliberately — the whole value
of a hatch like this is that it cannot be argued with at the moment you want to
argue with it, and a config key would put it one `tea settings` away from being
the thing it exists to prevent.

It is held rather than tapped because the page takes the keyboard the instant
it arrives, so whatever you were mid-sentence on lands here. A single keystroke
being the only cancel you get today is not a hatch, it is a trapdoor. Two
seconds is the same two seconds in a real emergency as it is in a weak moment;
the difference is that in the weak moment you have time to notice what you are
doing.

Nothing about it is hidden and nothing about it is advertised. The page shows
no sign of it until the key goes down — a break page that offers the way out on
every break is a break page suggesting it — and from then on it fills a line
under the word while you hold, so you can see what you are spending.

The break is cancelled, not postponed and not owed back: the work timer goes to
zero and the next break falls due a full interval from now, exactly as though
this one had been taken. That is the right shape for an emergency. What keeps
it honest is not making you pay it back, it is that there is one of them and
the log has it forever — `tea status` says the day's cancel is spent, `tea dash`
grows a **Cancelled** tile the first time you ever use one and a column in the
daily table, and Home Assistant gets a `rescue` event like any other. The
question worth answering is not whether you used it today, it is how the last
three weeks look.

## The page

The page as it ships is the default, pixel for pixel. `[page]` is for
departing from it on purpose:

    [page]
    accent = "#7aa2ff"      # the ring, the glow, the blast, the pills
    background = "dark"     # or "dim": the desk shows through, darkened
    font = ""               # a family name; empty takes the first monospaced face you have
    prompts = "off"         # "on", or a list of your own lines
    prompt_every = "20s"
    orb = "off"             # "on", or a list of your own: a lit circle naming what this break is for

`accent` recolours everything on the page that is not text — the text stays
white on dark, because it is meant to be read from the doorway. A colour that
does not parse costs a colour, not a break: the page uses the default and says
so once on startup.

`background = "dim"` leaves the desk visible through the dark, as shapes rather
than as anything you could read. A veil over the work, not a wall in front of
it. Some people find that easier to accept several times an hour; some find it
an invitation to squint. Try both.

`prompts` gives the page something to say while the clock runs. Off, it says
the one line it has always said. `"on"` cycles through a short built-in list —
look at something far away, roll your shoulders, drink some water — one line
every `prompt_every`, starting somewhere different each break. A list of your
own does the same with your words:

    prompts = ["Water.", "Look out of the window.", "Shoulders down."]

Only while the countdown runs. Once the page is waiting on the tag it has one
thing to say and says that.

`orb` puts an orb in the bottom-right corner — a lit circle in a warm
orange of its own, breathing slowly, its rim wandering — with one word in it:
what this break is for. Orange whatever the accent is, so it reads as a
different thing from the ring: one is the time, the other is what to do with it.
`"on"` deals from a built-in list of the parts of you that sit at a desk:
back, neck, shoulders, arms, wrists, eyes, hips, legs, and one break in nine
just to breathe. A different one each break, and every one of them before any
comes round again, so nine breaks are nine different stretches rather than
"Eyes" three times before lunch. A list of your own does the same with your
words:

    orb = ["Back", "Eyes", "Squats", "Stairs"]

The orb does not say what to do for your back. You know; it is there so the
break is a break for something in particular.

### What moves on it

Four things, and each of them is the page saying something it cannot say in
words to somebody who has stopped reading it. Five with the orb, which is the
one exception: it moves the whole time, slowly, in a box of its own.

- The **arrival**: the dark washes in, a blast goes out past the corners and
  the dial lands in the middle of it. `[animation] entrance` is how long that
  takes; `0` skips it and the page is simply there.
- The **last ten seconds**: a ghost of the ring pushes outward on each one,
  getting more insistent as they run out. Your desk is coming back and you
  should not have to be watching the numbers to know it.
- The **waiting page**, once the countdown is spent and the tag has not been
  scanned: a ring is pushed out from where the dial was, every few seconds,
  the way anything listening rather than counting ought to look. It stops the
  moment the gate opens — the confetti takes over — and a screen that can sit
  still for a ten-minute grace stops reading as a program that has died. It
  rests for longer than it moves, which is what makes it affordable.
- The **exit**: the ring draws itself back in and the page fades out from
  under it, over a share of whatever the entrance was set to. Switching the
  entrance off takes the exit with it.

And the **orb**, where there is one: its light swells and falls over four
seconds and its rim wanders a few pixels, two slow waves running round it in
opposite directions so it never quite repeats. Fifteen frames a second over a
box the size of a coaster, on its own clock rather than the page's.

And on a break that is being walked off, each square of the step meter lands
rather than appears — a little too big, with a glow, settling into the grid.
A batch of twenty arriving at once is dealt along the row rather than flashed,
so twenty reads as twenty.

None of it loops, nothing on this page ever strobes, and every part of it goes
quiet when it has nothing to say: the countdown is looked at eight times a
second for most of a break, and the frame clock is only asked for while
something is genuinely moving.

## The tag on the wall

The break page is scrupulous about time and completely blind to you. You can
sit through all five minutes of it without moving, and it will never know.
Waiting out a countdown at your own desk is not a break; it is a pause in the
typing.

`nfc.mode = "on"` makes the page wait for evidence instead. Put an NFC tag
somewhere you have to stand up and walk to — the kitchen, the hall, the other
end of the flat. The page now lifts when two things are true: the time has run
out, **and** the tag has been scanned. `tea set-nfc on` switches that on. If
your hub knows your step count, [a third](#and-twenty-steps) can be added: that
the walk actually happened.

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

### And twenty steps

A tag proves you stood up. It does not prove you went anywhere, and a tag stuck
within arm's reach of the chair proves nothing at all — the walk is the part
that matters, and the tag is only the thing that can witness it. If your hub
already knows your step count, it can witness the walk too:

    [nfc.steps]
    mode = "on"
    count = 20
    entity = "sensor.pixel_daily_steps"

Now the page lifts when *three* things are true: the time has run out, the tag
has been scanned, and twenty steps have been walked since the page appeared.
Neither half of the gate ends a break on its own.

Counted from where you were standing when the break started, so a running daily
total is exactly the right kind of sensor — the value at the first poll is the
yardstick, and only what accumulates on top of it counts.

The *first* rise is not credited either, and that is deliberate. A phone reports
steps when it syncs, not when they were walked, so the reading a break starts
from is whatever was last uploaded — minutes or hours old — and the next sync
arrives carrying everything walked since then, most of it from before the page
went up. Counting that would open the gate from the chair, which is the hole the
steps exist to close. So the first sync of a break moves the mark instead, the
walk is counted from there, and the badge says *Counting from here* rather
than *0 of 20 steps* — a page reporting nothing to somebody who has just
crossed the flat is a page that sends them across it again. The cost is whatever you
walked between the break starting and that sync; if your phone syncs rarely
enough that no second one arrives, `grace` ends the break on the clock.

That bargain is the right one for a feed that lags and the wrong one for a
sensor that keeps up. The phone's own step counter — the Home Assistant
companion app's *Steps sensor*, off by default in the app, reported every
minute or so once its update frequency is set to *fast always* — is a minute
old at most when the page goes up, and a minute earlier you were in the chair.
Its first rise is the walk, and moving the mark on it sends you across the flat
twice. Say so, and every rise counts:

    [nfc.steps]
    mode = "on"
    count = 20
    entity = "sensor.pixel_steps_sensor"
    sync = "live"

That counter runs from the phone's last reboot rather than from midnight, which
changes nothing here: the total is only ever measured from, never credited.
`sync` defaults to `"batched"` because that is the setting that cannot open the
gate from the chair; say `"live"` only of a sensor that actually keeps up.
A total that goes
*down* — midnight, a phone that re-pairs, a duplicate source dropped — is never
credited as steps: the new reading simply becomes the mark to measure from, and
the walk so far stands. Telling a rollover from a correction on one reading is
guesswork, and guessing generously would hand out a day's steps at once, so
neither is credited; at a real rollover that costs whatever was walked between
two polls, which is a couple of seconds of it. The same sensor is read on the same
beat as the tag, from the same hub, so this costs one more request every couple
of seconds while a page is up and nothing at all the rest of the time.

The page grows a second badge under the tag's: a small figure mid-stride, and
*12 of 20 steps* beside it, which becomes a green tick and *20 steps walked*
when the walk is in. Under the badges is a row of squares, one lighting per
step — the same count again, in the one form that can be read from the doorway
without your glasses on. Fifty to a row, so a `count` of a hundred is two rows
and a hundred and twenty is two rows and twenty, and a filled row is worth
exactly fifty steps wherever you are standing. Once the tag is scanned and only the steps are
left, the big text stops asking for the tag and starts counting down what is
actually outstanding — *8 more steps* — because a page still saying "scan the
tag" to somebody who has just scanned it is a page that sends them back down
the hall for nothing.

**This half degrades like everything else.** A step sensor that cannot be read
at all — renamed, deleted, a hub that has gone away — is a gate with a half
that can never close, so the break ends on its countdown. (The page says the
source is gone only while the tag is still outstanding: once a scan is in, the
badge that would carry the bad news is already green and stays that way, so the
first you hear of it is the page lifting early. Worth knowing, and the reason
`tea --probe` is the thing to run after renaming a sensor.) A sensor that simply
has nothing new to say is different and
treated differently: phones report steps in batches, and a count that has not
arrived yet must not undo steps already counted. If it never arrives, `grace`
does what it always does and hands the desk back. Worth knowing which one you
have before you rely on this: `tea --probe` prints the step sensor's current
value along with the tag's, and a value with a timestamp hours old is a phone
that syncs when it feels like it, not a gate you want in front of your screen.

Both halves are lost if the service restarts mid-break — the scan and the step
baseline together — because half a gate carried across a restart would let the
other half be walked twice. That is a deliberate difference from a plain tag
break, where the scan does survive.

### And thirty seconds on your feet

Steps close most of the hole the tag leaves, not all of it: a phone waved at
the desk earns steps. Android's activity recognition wants the whole body going
somewhere, and the companion app reports its word as an entity —
`sensor.<phone>_detected_activity`, saying `walking`, `still`, `in_vehicle`.
The third half of the gate asks for a little time in a moving state:

    [nfc.moving]
    mode = "on"
    entity = "sensor.pixel_detected_activity"
    for = "auto"
    states = ["walking", "on_foot", "running"]

Read from the hub on the same beat as the steps. Every answer in one of those
states is worth one poll's beat, added up over the break; a `still` between
two `walking`s takes nothing away. The page grows a third badge beside the tag
and the steps — *Not moving yet*, *Moving · 12s of 30s*, then green *Moved* —
and once the countdown is spent and only this half is missing, it says *Keep
walking — 18s more* rather than sending you back to the tag.

`for = "auto"` scales the time from the steps: a hundred steps is a minute of
walking and asks for thirty seconds of the phone saying so, ten steps asks for
five, never less than five nor more than two minutes. Half of the walk rather
than all of it, because the sensor is late and lumpy. A duration says it
outright, and thirty seconds is what auto means when no steps are counted.

And a hand is not a walk. The two sensors disagree in exactly one way that
only a shaken phone produces: the step count climbs while the activity sensor
keeps saying `still`. Once twenty such steps have arrived and half a minute
has gone by with no movement seen, the page says so — the steps badge turns
amber and reads *Nice try* with the count crossed out, the line under the
clock becomes *That was the phone walking, not you*, and the day's tally
counts it (`sensor.tea_cheats_today` on the hub, *Nice tries* on the
dashboard). The gate itself is not changed: it is still waiting for the walk,
and the teasing stops the moment the phone reports moving.

No hardware: switch on the *Detected activity* sensor in the companion app.
Android reports it lazily, a minute behind at times, so `for` is a floor and
not a stopwatch, and a sensor that cannot be read turns the badge amber and
ends the break on the clock, the way the tag does. `tea --probe` reads it.

### Asking the phone to hurry up

Both of those halves wait on a phone, and a phone reports its sensors when it
feels like it. A minute is the good case — the companion app's update frequency
set to *fast always* — and a minute is a long time to stand in the hall after
the walk is over, in front of a page that has not heard about it yet. The gate
is right; the news is late.

The Android companion app has a way round that, and it is a notification. A
message of `command_update_sensors` makes the app report every sensor it has,
at once. Name the phone and tea sends one:

    [nfc.home_assistant]
    url = "http://homeassistant.local:8123"
    entity = "tag.living_room"
    nudge = "notify.mobile_app_pixel"

Ten seconds into the break, and every thirty seconds after that for as long as
the page is up. The first ten are yours — a phone asked about a walk that has
not started has nothing to say — and after them the count arrives within half a
minute of the phone having it, rather than on the phone's next minute. Not more
often than that: a phone in a pocket with the screen off holds most pokes back
anyway, and the count it reports is written a minute or two apart, so a poke
every ten seconds was a pocket buzzed about a number that had not changed. `mobile_app_pixel` without the `notify.` in
front does the same thing: the service is the name your phone registered with,
and `notify.` is the only domain it could be in.

It stops the moment the gate has what it wants. The walk usually comes in with
minutes of the break still to run, and a fresh reading after that is a
notification about a number nothing is reading any more — so the poking ends
with the walk and starts again only if the count goes backwards and the walk
with it.

Nothing waits on it. The poke goes out on its own errand rather than joining the
poll's queue of questions, so a hub that has gone slow costs the gate nothing,
and a phone that cannot be told — never set up, renamed, logged out — costs one
line on stderr and a break that ends exactly as it would have without any of
this. It is sent only while a break is on screen, and only when something is
actually reading that phone: with `[nfc.steps]` and `[nfc.moving]` both off
there is nothing to wake it for, and tea says so at startup rather than
notifying your pocket every half minute for nobody.

### And something to do with it

Everything above is about making you get up. This is the only part about what
to do once you have — because a page that says *stand up* and nothing else
leaves you standing in the kitchen wondering why you are there. Point tea at a
Home Assistant to-do list:

    [nfc.chores]
    mode = "on"
    entity = "todo.household"
    title = "While you're up"
    show = 8

and the break page grows a list in the top-left corner:

    While you're up
    ├─ Luft
    ├─ Tidy up the kitchen
    ├─ Clean the rack
    ├─ 2 more
    ├─ Clean windows
    └─ Take out the bins
    ─────────────
    today · 2 tasks done

The open ones first, in the list's own order, then the ones already done —
struck through, green, and fading as they go down. The last two lines are kept
for finished jobs whenever there are any, so a list with eleven things still on
it cannot fill the corner with nothing but work; when there is little left to
do, the finished ones take the slack instead of leaving the page half empty.
`title` is what goes above them, and defaults to the entity id, which is what
the list is called and nobody's idea of a heading.

Whatever does not fit is counted on a line of its own — *3 more*, in the same
grey as the branches, so it reads as arithmetic rather than as a job you could
go and do. That line is what keeps the corner honest: what you can see plus what
it says it is hiding **is** the list, and no number on the page can disagree with
the rows beneath it. It costs no job its place, so `show = 8` means eight jobs.

Under a hairline, once the day has something to report, the day itself: *today ·
2 tasks done*, the count in the same green the finished jobs are struck through
in. The rule and the leading word are doing a job — above the line is this list
now, below it is you today — because a bare number beside a sample of a list
invites you to count the rows, and the rows will not add up to it. It says
nothing at all until the first job goes green; a corner opening with *0 done
today* would be nagging you before you had stood up. When there is genuinely
nothing left it says *list clear* instead, which is the one moment the corner
gets to be pleased with you. The first line is the only
bright one on the page besides the clock: it is the job you would pick, and the
page has exactly one thing to catch an eye that is supposed to be leaving the
screen.

It is read *live*, every few seconds, and it is read-only. Tick a job off on
your phone while you are standing at the window and a strike is drawn across
the line, left to right, the words warming to green as it goes; then the row
drops into the finished pile while the rows that were under it slide up to
close the gap, and nothing below the pile moves at all. The list itself grows
in like a tree when the page arrives — branch after branch down the trunk,
each name sliding in a beat behind its branch — and a deadline inside the
hour breathes, a slow swell of its glow, the one moving thing in the corner.
Tick a job off and the line goes green behind you; tea will not tick anything off on your behalf, because a job marked done
from the laptop you were just sent away from is a claim nothing here can check.

What is still to do is always at the top, and what has been done is always
underneath it, freshest first. Among the open ones, anything with a due date
comes first, soonest at the top, drawn warm and lit with the time after the
name — `16:30` today, `Fri 16:30` this week, `4 Sep` beyond — and reddening
once it has gone by. The rest follow in the list's own order. Tick a job off and it
drops below the open ones; its row goes to the next job that did not fit, and
the *N more* marker shrinks to match. A job that never fit on screen still
counts if you do it.

None of this is part of the gate. The list holds nothing back, ends no break
early, and a hub that cannot be asked about it costs one line on stderr and an
empty corner — there is no error text on a page whose whole job is to be
restful.

Whatever gets ticked off while a page is up is counted. `tea status` says how
many today, `tea dash` has them per day and in total, and the hub gets
`sensor.tea_chores_today` beside the rest.

### Telling Home Assistant

Everything above is the hub talking to tea. This is tea talking back:

    [nfc.home_assistant]
    url = "http://homeassistant.local:8123"
    token_file = "~/.config/tea/.env"
    publish = "on"
    publish_entity = "sensor.tea"

With that on, the hub has a sensor whose state is what tea is doing — `working`,
`warning`, `held`, `break`, `waiting`, `off` — with the rest hanging off it as
attributes: `next_break_at` and `break_ends_at` as timestamps, `steps_walked`
and `steps_needed`, `tag_scanned`, `postpones_left`, today's `breaks_today` and
`steps_today`, and `why_off` when it is asleep. And at each turn it fires a
`tea` event with `what` set to the turn: `warning`, `break_start`, `waiting`,
`scan`, `released`, `break_end`, `postpone`, `credited`, `rescue`, `held`,
`off`, `on`.
The hall light is an automation on `event_type: tea` with `event_data:
{what: break_start}`; the speaker is the same with `released`.

The hub cannot graph an attribute, so beside the sensor go six plain numbers,
each with a unit and a state class so the recorder keeps long-term statistics
for them: `sensor.tea_worked` (minutes towards the next break),
`sensor.tea_walk` (steps this break), `sensor.tea_steps_today`,
`sensor.tea_breaks_today`, `sensor.tea_postpones_today`, and
`binary_sensor.tea_break` (on while the page is up). Named after
`publish_entity`, so `sensor.desk` gets `sensor.desk_steps_today`.

`dist/home-assistant/tea-dashboard.yaml` is a dashboard built on them, kept
to five numbers and two pictures: breaks, steps in breaks, all steps, and
work today; the day's walking as two rising lines, steps in breaks against
the phone's own count; breaks as a bar a day for a month; and the same three
numbers added up since tea started reporting, work as minutes and hours both
(`1,020 min (17.0 h)`, off two helpers `push.py dashboard` makes: a utility
meter over `sensor.tea_worked_today` and a template sensor that spells it out).
The phone's step sensor is the one entity in it that is yours to fill in. For Settings → Dashboards → Add
dashboard → raw configuration editor, or `push.py` below. `dist/home-assistant/automations.yaml`
is the hall light, the speaker, and the phone, on the events above. Neither
needs the clipboard: `dist/home-assistant/push.py dashboard` creates or
replaces the dashboard over the hub's API, `push.py list` says what the hub
has to point automations at, and `push.py automations tea_held tea_summary
notify.mobile_app_phone=notify.mobile_app_yours` adds those two with the
placeholder filled in.

It needs `url` and a token, not the tag: reporting works with `nfc` off, for a
hub that only ever hears from tea and is never asked anything. The token has
to be allowed to write, which a long-lived access token is.

Sent when something changes and not otherwise. The hub is told about a new
state, a scan, a step count that moved, and once a minute that the clock is
still running; it is not told the time left every second, because it can do
that sum from `break_ends_at` for itself. A hub that cannot be reached costs
one line on stderr and a retry half a minute later, and the break carries on
exactly as it would have. The sensor is tea's, not the hub's: a hub restarted
forgets it, and gets it back on tea's next report, within the minute.

    tea --probe

fires a `tea` event with `what: probe` and says whether the hub took it.

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

`listen`, under `[port]`, defaults to loopback, which answers this machine and
nothing else. A phone in another room needs one of two things.

**Bind it to the network.** `listen = "0.0.0.0:9797"` under `[port]`, plus — on Ubuntu, where
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

    [port]
    listen = "127.0.0.1:9797"

    [nfc]
    url = "https://tea.example.home/unlock"

Scans then arrive from the proxy rather than the phone, so the log names
whoever the proxy says it was carrying — `X-Forwarded-For`, for the log and
nothing else. A header is a claim, never an authorisation.

## The settings page

Everything in the config file, in a browser, without the browser owning the
file:

```
tea settings
```

does whatever is still needed — switches the page on in the file, writes a
token under `[port]` if there is none, restarts the service so it hears about
both — and opens `http://127.0.0.1:9797/settings` with the token in the
address. No tag is involved and none is needed.

The page shows the file one setting to a row, two columns of cards on a wide
screen — the comment beside each one as its help, `"on"`/`"off"` as a switch,
everything else as text — with a search box at the top that narrows it to the
rows whose name, section, help or current value mention what you typed, and a
pill beside the mark saying what tea is doing while you edit it.

It shows the settings your file has *never* mentioned as well, which is the
only way some of them can be reached at all: a file with no `[hours]` in it
has no working hours to edit, and a page that can only draw the lines you
already have has nowhere to put them. So every setting tea has is drawn from
the file as it ships — dashed and dim, with what it would be if you never
said sitting in the box as a placeholder and the shipped file's own words
about it alongside. Type in one and it turns solid: a line about to be
written. A section you have never touched is a card of its own, marked *not
set*, and gains a `[section]` heading in your file the moment you fill in a
row of it.

*Save* sends the whole file back; tea parses it the way it parses it at
start-up, refuses it with the line and column if it would not load, and writes
it in one move if it would. *Apply changes* does the same and then restarts tea
on it, the way `tea reload` does; it appears whenever there is something to
apply. Comments, blank lines and column alignment survive a save: the page only
ever swaps the value on the lines you touched, and a line it adds goes at the
end of its own section with the shipped file's note in the same column the rest
of the file keeps its comments in.

Nothing new is listening. The page is served on `[port]`, the same socket
that hears the tag when tea does its own listening, behind the same token —
sent once in the address, taken out of it again, and carried as a header on
every request after, which is what keeps any other page open in your browser
from writing a config here.
It costs nothing while nobody is looking at it: a socket nobody connects to
never wakes the main loop.

```toml
[settings]
page = "on"
```

Off by default, because an upgrade must never quietly put a file editor on a
port; `tea settings` is the one thing that turns it on, and only when asked.
Loopback only unless `[port] listen` says otherwise; if it does, the page is
reachable from wherever the tag is, behind the same token.

Where this page is on, the dashboard below is served beside it at `/dash` on
the same port and behind the same token, and the two link to each other in
their headers. Served, its numbers are gathered per request rather than frozen
at the moment a file was written. It rides on this switch rather than having
one of its own: wanting to look at a chart is not a reason to open a port.

## Status

    $ tea status

      tea — working

      work      ██████████████░░░░░░░░░░   60%
                15m of 25m — next break in 10m

      postpone  1 of 2 left — resets in 40m
      today     3 breaks · 1 away from the desk · 187 steps walked · 2 tasks done
      long      15m — after 2 more ordinary breaks
      now       idle 4s · nothing is holding a break
      service   running · saved 2s ago
      config    ~/.config/tea/config.toml

Read from the state file the service already writes, so it is accurate to
within one save interval. It shows the config file's values — if you edit the
config while the service is running, restart it before trusting the numbers.

The `today` row is the only line here that says whether any of this is
working; everything above it is about the next five minutes. It counts breaks
that ran, absences credited as breaks, postpones spent, steps walked and jobs
ticked off the list, and it starts again at local midnight. While tea is off or out of hours the gauges
are replaced by the reason, because a work bar that has not moved since Friday
is worse than no bar at all.

## The last few weeks

    $ tea dash

Everything above answers *the next five minutes*, and the `today` row starts
again at midnight — which is right for a status line and useless for the only
question that matters after the first week: whether any of this is working.

So a break that ends writes a line to
`$XDG_STATE_HOME/tea/history.jsonl` — when it was, how long it ran, how far it
was walked, and whether the tag was the thing that ended it. Postpones and
credited absences get a line each too. `tea dash` reads that log, builds a page
out of it and opens it:

- **five tiles across the top** — today, breaks, steps a break, tasks done and
  the streak — each with where that number stood one window ago beside it, up or
  down, with both raw counts under the pointer. A window the log does not reach
  all the way back through gets no comparison rather than an invented one, so
  the chips appear when there is a like for like and not before.
- **steps per break**, one dot each, against the line the gate asks for. The
  chart that says whether the walks are walks or a tag within reach of the chair.
- **breaks a day**, split into the ones the page ran and the ones you had
  already earned by being away, with postpones on a track beneath.
- **steps a day**, and the week around each one.
- **when breaks happen**, by hour. A hollow afternoon is an afternoon you dodged.
- **how breaks ended** — walked, scanned, or handed back on `grace`.

It is a file first. Nothing listens on its account, no port opens for it, and
nothing is fetched from the network to draw it: the page is written to
`$XDG_STATE_HOME/tea/dash.html`, and that is the copy you keep — mail it to
yourself, open it on a plane. `--no-open` prints the path and stops there,
which is what you want over SSH.

Where the settings page above is switched on there is a port already, and the
same page is served on it at `/dash` behind the same token — one link from the
settings page, and one link back. That copy reads its numbers when you ask for
them rather than when a file was written, and it carries a *Refresh* of its
own, because the token is taken back out of the address bar the moment the
page has read it.

Its header carries a *Home Assistant* link too, the same one the settings page
has, wherever `[nfc.home_assistant] url` is set. The hub is a service of its
own on its own address and needs nothing of tea's to reach, so that link works
from the written file as well as from the served copy — the address goes on the
page, the token never does.

`tea dash` writes the file either way and opens **that** copy in preference to
it, wherever the daemon is up to answer — the file is the one copy of this page
with nowhere much of its own to go. It cannot carry the token, deliberately: the token is kept
out of a file that gets mailed and screenshotted, which is the same reason
anything called `token` is blanked out of the config quoted at the foot of it.
So opened on its own the file names `tea settings` in its header rather than
pretending to link there, and where no page is switched on it says nothing at
all.

The file carries your config at the foot of it, read-only, with anything called
`token` blanked out on the way in — and it is written `chmod 600` regardless,
because the config it quotes is. Read-only because a `file://` page cannot write
anything back; settings are still changed with `tea set-work 30m` and
`tea reload`.

The log starts the moment a version that writes it is the one running, and
nothing can reconstruct the weeks before that. The page says so rather than
drawing empty axes.

## Layout

    crates/core   pure state machine + the `Blocker` trait — no clock, no I/O
    crates/cli    host: config, clock, D-Bus session, GTK overlay, terminal,
                  and the two ends of the tag: a Home Assistant poll, and a
                  small HTTP ear for anything that would rather push
    dist/         systemd user unit + install script

`crates/cli/src/dash.html` is the dashboard, embedded with `include_str!` and
filled in at the one placeholder — a real file you can open and edit rather
than a string built in Rust.

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
- **The log is never load-bearing** — `history.jsonl` is appended to
  best-effort, complained about once if it cannot be, and read back a line at a
  time. A half-written line from a `kill -9` costs that break and nothing else.
  A chart must never be the reason a break does not happen.
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
  players and presentation mode set. Something of your own that holds the
  screen awake for a long job is not a call: list a piece of its app id or
  reason under `calls.ignore` and it is passed over, in the timer, in `tea
  status` and in `tea --probe` alike.
- **Degrades, never fails** — no session bus (SSH, gnome-shell restart) costs
  accuracy, not availability: it falls back to suspend-gap detection and says so
  once.
- **Postpone is budgeted** — 2 per hour, only once a break is imminent.
  Unlimited snooze is the same as no tool.
- **One bad answer is not an outage** — the hub is asked for the tag once every
  couple of seconds, so a dropped packet or a Home Assistant mid-reload is
  routine. It takes three unanswered polls in a row to call the source gone and
  end the break on its countdown. Acting on the first would have the gate that
  exists to make you walk quietly opening itself, a few times a week.
- **A scan is proof of going, not a way out** — with the tag on, an early scan
  never shortens a break and a late one never lengthens it. All it decides is
  whether the page lifts when the countdown does.

## Roadmap

- [x] **P0** state machine + CLI host
- [x] **P1** TOML config, GTK4 overlay, `systemd --user` unit
- [x] **P2a** real idle via `org.gnome.Mutter.IdleMonitor` + inhibitor detection
- [x] **P2b** break debt persisted to `$XDG_STATE_HOME`
- [x] **P2c** the tag on the wall — a break the countdown alone cannot end
- [x] **P2d** and the walk to it, counted in steps
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
    tea run 5s --hold soft     # ...without the file's strict hold, for the length of the try
    tea run 5s --no-gate       # ...and without waiting for the tag and the walk afterwards
    tea run-warning            # show the warning toast
    tea reload                 # pick up edited settings
    tea status                 # what the running service is doing
    tea config                 # every setting, nicely laid out
    tea dash                   # steps, breaks and how the habit is going
    tea dash --no-open         # ...write the page without opening it
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

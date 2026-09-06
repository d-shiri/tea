#!/usr/bin/env python3
"""Put tea's dashboard on a Home Assistant hub, without the clipboard.

    ./dist/home-assistant/push.py dashboard sensor.phone_daily_steps=sensor.<yours>_daily_steps
                                                     # create or replace the "tea" dashboard,
                                                     # with your phone's step sensor filled in
    ./dist/home-assistant/push.py automations tea_held tea_summary \\
        notify.mobile_app_phone=notify.mobile_app_sm_g990b2
                                                     # add those automations, with the
                                                     # placeholder entities replaced
    ./dist/home-assistant/push.py list               # what the hub has to point them at

Reads the hub's address from tea's config and the token from wherever tea reads
it: `token`, then `token_file`, then $TEA_HA_TOKEN. Needs the `websockets`
and `pyyaml` packages, which most distributions have.
"""
import asyncio
import json
import os
import re
import sys
import urllib.error
import urllib.request
from datetime import datetime, timedelta, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
DASHBOARD = HERE / "tea-dashboard.yaml"
AUTOMATIONS = HERE / "automations.yaml"
# A dashboard's path has to have a hyphen in it; the hub insists.
URL_PATH = "tea-breaks"


def config_path():
    base = os.environ.get("XDG_CONFIG_HOME") or str(Path.home() / ".config")
    return Path(base) / "tea" / "config.toml"


def hub():
    """(url, token) the way tea itself resolves them."""
    import tomllib
    with open(config_path(), "rb") as f:
        ha = tomllib.load(f).get("nfc", {}).get("home_assistant", {})
    url = ha.get("url", "").rstrip("/")
    if not url:
        sys.exit("tea: nfc.home_assistant.url is not set")
    token = ha.get("token", "").strip()
    if not token and ha.get("token_file"):
        text = Path(os.path.expanduser(ha["token_file"])).read_text()
        m = re.search(r"^\s*(?:export\s+)?TEA_HA_TOKEN\s*=\s*['\"]?([^'\"\n]+)", text, re.M)
        token = m.group(1).strip() if m else text.strip()
    token = token or os.environ.get("TEA_HA_TOKEN", "")
    if not token:
        sys.exit("tea: no token — set nfc.home_assistant.token, token_file, or $TEA_HA_TOKEN")
    return url, token


async def talk(url, token, requests):
    """Send a list of websocket commands, return their results in order."""
    import websockets

    ws_url = re.sub(r"^http", "ws", url) + "/api/websocket"
    async with websockets.connect(ws_url, max_size=8 * 1024 * 1024) as ws:
        assert json.loads(await ws.recv())["type"] == "auth_required"
        await ws.send(json.dumps({"type": "auth", "access_token": token}))
        reply = json.loads(await ws.recv())
        if reply["type"] != "auth_ok":
            sys.exit(f"tea: the hub refused the token — {reply.get('message')}")
        results = []
        for i, req in enumerate(requests, start=1):
            await ws.send(json.dumps({"id": i, **req}))
            while True:
                reply = json.loads(await ws.recv())
                if reply.get("id") == i:
                    break
            if not reply.get("success"):
                sys.exit(f"tea: {req['type']} failed — {reply.get('error')}")
            results.append(reply.get("result"))
        return results


def dashboard(url, token, args=()):
    import yaml

    text = DASHBOARD.read_text()
    # `old=new` pairs fill in the one entity that is yours: the phone's steps.
    for old, new in (a.split("=", 1) for a in args if "=" in a):
        text = text.replace(old, new)
    for holder, hint in (
        ("sensor.phone_daily_steps", "sensor.phone_daily_steps=sensor.<yours>_daily_steps"),
        ("todo.household", "todo.household=todo.<yours>"),
    ):
        if holder in text:
            print(f"tea: note — {holder} is a placeholder; pass {hint} to fill it in")
    config = yaml.safe_load(text)
    (existing,) = asyncio.run(talk(url, token, [{"type": "lovelace/dashboards/list"}]))
    if not any(d.get("url_path") == URL_PATH for d in existing):
        asyncio.run(
            talk(
                url,
                token,
                [
                    {
                        "type": "lovelace/dashboards/create",
                        "url_path": URL_PATH,
                        "title": config.get("title", "tea"),
                        "icon": "mdi:tea",
                        "show_in_sidebar": True,
                        "require_admin": False,
                    }
                ],
            )
        )
        print(f"tea: created the dashboard at {url}/{URL_PATH}")
    else:
        print(f"tea: replacing the dashboard at {url}/{URL_PATH}")
    worked_helpers(url, token)
    asyncio.run(talk(url, token, [{"type": "lovelace/config/save", "url_path": URL_PATH, "config": config}]))
    print("tea: done — it is in the sidebar")


def rest(url, token, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(
        url + path,
        data=data,
        method="POST" if data else "GET",
        headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req) as reply:
        return json.load(reply)


def worked_helpers(url, token):
    """The two helpers behind the "Worked" card under "Since the start".

    A utility meter adds `sensor.tea_worked_today` up across its midnight
    resets, and a template sensor spells the total out as `1,020 min (17.0 h)`,
    which a statistic card cannot. Made once; the meter is seeded from what the
    recorder has already added up, so the total does not start again at zero.
    """
    import time

    def state(entity):
        try:
            return rest(url, token, "/api/states/" + entity)["state"]
        except urllib.error.HTTPError:
            return None

    def flow(handler, *steps):
        f = rest(url, token, "/api/config/config_entries/flow", {"handler": handler})
        for step in steps:
            f = rest(url, token, "/api/config/config_entries/flow/" + f["flow_id"], step)
        if f.get("type") != "create_entry":
            sys.exit(f"tea: the hub refused the {handler} helper — {f.get('errors') or f}")

    def wait_for(entity):
        for _ in range(40):
            if state(entity) is not None:
                return
            time.sleep(0.5)
        sys.exit(f"tea: {entity} never showed up")

    if state("sensor.tea_worked_total") is None:
        flow(
            "utility_meter",
            {
                "name": "tea worked total",
                "source": "sensor.tea_worked_today",
                "cycle": "none",
                "offset": 0,
                "tariffs": [],
                "net_consumption": False,
                "delta_values": False,
                "periodically_resetting": True,
                "always_available": False,
            },
        )
        wait_for("sensor.tea_worked_total")
        # Seed: every past day's change per the recorder, plus today's counter.
        start = (datetime.now(timezone.utc) - timedelta(days=400)).isoformat()
        (stats,) = asyncio.run(
            talk(
                url,
                token,
                [
                    {
                        "type": "recorder/statistics_during_period",
                        "start_time": start,
                        "statistic_ids": ["sensor.tea_worked_today"],
                        "period": "day",
                        "types": ["change"],
                    }
                ],
            )
        )
        today = datetime.now().date()
        past = sum(
            (row["change"] or 0)
            for row in stats.get("sensor.tea_worked_today", [])
            if datetime.fromtimestamp(row["start"] / 1000).date() != today
        )
        total = int(past + float(state("sensor.tea_worked_today") or 0))
        rest(url, token, "/api/services/utility_meter/calibrate", {"entity_id": "sensor.tea_worked_total", "value": str(total)})
        print(f"tea: made sensor.tea_worked_total, seeded to {total} min")
    if state("sensor.tea_worked_since_start") is None:
        flow(
            "template",
            {"next_step_id": "sensor"},
            {
                "name": "tea worked since start",
                "state": "{% set m = states('sensor.tea_worked_total') | float(0) %}"
                "{{ '{:,}'.format(m | int) }} min ({{ (m / 60) | round(1) }} h)",
            },
        )
        wait_for("sensor.tea_worked_since_start")
        print("tea: made sensor.tea_worked_since_start")


def automations(url, token, args):
    import yaml

    wanted = [a for a in args if "=" not in a]
    swaps = dict(a.split("=", 1) for a in args if "=" in a)
    text = AUTOMATIONS.read_text()
    for old, new in swaps.items():
        text = text.replace(old, new)
    items = yaml.safe_load(text)
    if not wanted:
        sys.exit("tea: say which — ids: " + ", ".join(i["id"] for i in items))
    unknown = set(wanted) - {i["id"] for i in items}
    if unknown:
        sys.exit("tea: no automation called " + ", ".join(sorted(unknown)))
    if "tea_tasks_done" in wanted:
        # That one lands its count in a number helper, which has to exist first.
        (states,) = asyncio.run(talk(url, token, [{"type": "get_states"}]))
        if not any(s["entity_id"] == "input_number.tea_tasks_done" for s in states):
            asyncio.run(
                talk(
                    url,
                    token,
                    [
                        {
                            "type": "input_number/create",
                            "name": "tea tasks done",
                            "min": 0,
                            "max": 10000,
                            "step": 1,
                            "mode": "box",
                            "icon": "mdi:clipboard-check-multiple-outline",
                            "unit_of_measurement": "tasks",
                        }
                    ],
                )
            )
            print("tea: made the helper input_number.tea_tasks_done")
    for item in items:
        if item["id"] not in wanted:
            continue
        left = re.findall(r"\b(?:light|media_player|notify|tts)\.[a-z0-9_]*(?:hall|phone|kitchen|cloud)\b", json.dumps(item))
        if left:
            sys.exit(f"tea: {item['id']} still has placeholders: " + ", ".join(sorted(set(left))) + " — pass old=new")
        slug = item["id"]
        body = json.dumps(item).encode()
        req = urllib.request.Request(
            f"{url}/api/config/automation/config/{slug}",
            data=body,
            method="POST",
            headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"},
        )
        with urllib.request.urlopen(req) as reply:
            json.load(reply)
        print(f"tea: added automation.{slug}")


def listing(url, token):
    states, services = asyncio.run(talk(url, token, [{"type": "get_states"}, {"type": "get_services"}]))
    for domain in ("light", "switch", "media_player", "tts"):
        ids = sorted(s["entity_id"] for s in states if s["entity_id"].startswith(domain + "."))
        if ids:
            print(f"{domain}:")
            for e in ids:
                name = next(s for s in states if s["entity_id"] == e)["attributes"].get("friendly_name", "")
                print(f"  {e}  {name}")
    notify = sorted(k for k in services.get("notify", {}) if k.startswith("mobile_app_"))
    if notify:
        print("notify:")
        for n in notify:
            print(f"  notify.{n}")


def main():
    what = sys.argv[1] if len(sys.argv) > 1 else ""
    url, token = hub()
    if what == "dashboard":
        dashboard(url, token, sys.argv[2:])
    elif what == "automations":
        automations(url, token, sys.argv[2:])
    elif what == "list":
        listing(url, token)
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Put tea's dashboard on a Home Assistant hub, without the clipboard.

    ./dist/home-assistant/push.py dashboard          # create or replace the "tea" dashboard
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
import urllib.request
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


def dashboard(url, token):
    import yaml

    config = yaml.safe_load(DASHBOARD.read_text())
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
    asyncio.run(talk(url, token, [{"type": "lovelace/config/save", "url_path": URL_PATH, "config": config}]))
    print("tea: done — it is in the sidebar")


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
        dashboard(url, token)
    elif what == "automations":
        automations(url, token, sys.argv[2:])
    elif what == "list":
        listing(url, token)
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Rebuilds rust/data/known-games.tsv.gz, the games Miyav recognises by their exe when
no store knows about the install. The data is Discord's public list of detectable
games.

Each line is "exe pattern<TAB>game name". A pattern is the exe's file name, or the end
of its path when the list gives the folder too ("terra invicta/terrainvicta.exe");
lowercase, with forward slashes.

    python3 tools/update_games.py             (needs the internet)
    python3 tools/update_games.py list.json   (from a copy saved earlier)
"""
import gzip
import json
import sys
import urllib.request

SOURCE = "https://discord.com/api/v9/applications/detectable"
OUT = "rust/data/known-games.tsv.gz"

# Exe names too generic to mean one game on their own. With a folder in front they
# are still kept: "ahegal/game.exe" is specific enough.
GENERIC = set("""
    game.exe launcher.exe start.exe play.exe client.exe main.exe app.exe setup.exe run.exe
    java.exe javaw.exe python.exe pythonw.exe node.exe electron.exe nw.exe love.exe godot.exe
    dosbox.exe scummvm.exe retroarch.exe hl2.exe steam.exe""".split())


def helper(base):
    """Programs that live in game folders but are never the game's window."""
    return base.startswith(("unitycrashhandler", "crashreport", "unins")) or "crashpad" in base


def load():
    if len(sys.argv) > 1:
        with open(sys.argv[1], encoding="utf-8") as f:
            return json.load(f)
    req = urllib.request.Request(SOURCE, headers={"User-Agent": "miyav-update-games"})
    with urllib.request.urlopen(req, timeout=60) as res:
        return json.load(res)


def main():
    games = {}  # pattern -> game names
    for app in load():
        name = " ".join((app.get("name") or "").split())
        if not name:
            continue
        for exe in app.get("executables") or []:
            p = (exe.get("name") or "").strip().replace("\\", "/").lower()
            # ">" marks an exe told apart by its arguments; the name alone would be
            # wrong for all but one of its games.
            if exe.get("os") != "win32" or exe.get("is_launcher") or p.startswith(">") or not p.endswith(".exe"):
                continue
            p = p.strip("/")
            base = p.rsplit("/", 1)[-1]
            if helper(base) or (p == base and p in GENERIC):
                continue
            games.setdefault(p, set()).add(name)

    # A pattern claimed by several games names none of them for sure.
    lines = sorted(f"{p}\t{next(iter(names))}" for p, names in games.items() if len(names) == 1)
    with open(OUT, "wb") as raw, gzip.GzipFile(fileobj=raw, mode="wb", mtime=0) as gz:
        gz.write(("\n".join(lines) + "\n").encode("utf-8"))
    print(f"{len(lines)} patterns, {len(games) - len(lines)} ambiguous ones dropped")


if __name__ == "__main__":
    main()

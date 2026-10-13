#!/usr/bin/env -S python3 -I
import json
import os
import plistlib
import sys

STATE_DIR = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "state")
CALLS = os.path.join(STATE_DIR, "calls.jsonl")
LOADED = os.path.join(STATE_DIR, "loaded")
SCRIPT = os.path.join(STATE_DIR, "script.json")


def scripted(sub):
    try:
        with open(SCRIPT) as f:
            script = json.load(f)
    except FileNotFoundError:
        return None
    queue = script.get(sub) or []
    if not queue:
        return None
    reply = queue.pop(0)
    script[sub] = queue
    with open(SCRIPT, "w") as f:
        json.dump(script, f)
    return reply


def modelled(args):
    sub = args[0] if args else ""
    loaded = os.path.exists(LOADED)
    if sub == "print":
        if loaded:
            with open(LOADED) as f:
                return {"exit": 0, "stdout": f.read()}
        return {"exit": 113, "stderr": f'Bad request.\nCould not find service "{args[1]}" in domain\n'}
    if sub == "bootout":
        if loaded:
            os.remove(LOADED)
            return {"exit": 0}
        return {"exit": 3, "stderr": "Boot-out failed: 3: No such process\n"}
    if sub == "bootstrap":
        if loaded:
            return {"exit": 5, "stderr": "Bootstrap failed: 5: Input/output error\n"}
        with open(args[2], "rb") as f:
            label = plistlib.load(f)["Label"]
        with open(LOADED, "w") as f:
            f.write(f"{args[1]}/{label} = {{\n\tstate = running\n\tpid = 4242\n}}\n")
        return {"exit": 0}
    return {"exit": 64, "stderr": f"Unrecognized subcommand: {sub}\n"}


def main():
    args = sys.argv[1:]
    with open(CALLS, "a") as f:
        f.write(json.dumps({"argv": args, "loaded": os.path.exists(LOADED)}) + "\n")
    reply = scripted(args[0] if args else "") or modelled(args)
    sys.stdout.write(reply.get("stdout", ""))
    sys.stderr.write(reply.get("stderr", ""))
    return reply.get("exit", 0)


sys.exit(main())

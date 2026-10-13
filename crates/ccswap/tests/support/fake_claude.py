#!/usr/bin/env -S python3 -I
import json
import os
import sys

STATE_DIR = @STATE_DIR@
CALLS = os.path.join(STATE_DIR, "calls.jsonl")
RESPONSES = os.path.join(STATE_DIR, "responses.json")


def main():
    args = sys.argv[1:]
    with open(CALLS, "a") as f:
        f.write(json.dumps({"argv": args, "cwd": os.getcwd(), "env": dict(os.environ)}) + "\n")
    try:
        with open(RESPONSES) as f:
            responses = json.load(f)
    except FileNotFoundError:
        responses = {}
    reply = responses.get(" ".join(args), {})
    sys.stdout.write(reply.get("stdout", ""))
    sys.stderr.write(reply.get("stderr", ""))
    return reply.get("exit", 0)


sys.exit(main())

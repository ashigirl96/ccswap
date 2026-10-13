#!/usr/bin/env -S python3 -I
import json
import os
import shlex
import sys

STATE_DIR = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "state")
ITEMS = os.path.join(STATE_DIR, "items.json")
CALLS = os.path.join(STATE_DIR, "calls.jsonl")
FAIL_RC = os.path.join(STATE_DIR, "fail_rc")

NOT_FOUND = 44
DUPLICATE = 45
VALUE_OPTS = {"-a", "-s", "-w", "-X", "-l", "-D", "-j", "-c", "-C", "-G", "-T", "-r", "-k"}


def load():
    try:
        with open(ITEMS) as f:
            return json.load(f)
    except FileNotFoundError:
        return []


def save(items):
    tmp = ITEMS + ".tmp"
    with open(tmp, "w") as f:
        json.dump(items, f, indent=2)
    os.replace(tmp, ITEMS)


def parse(cmd, args):
    value_opts = VALUE_OPTS if cmd == "add-generic-password" else VALUE_OPTS - {"-w"}
    opts, flags = {}, set()
    i = 0
    while i < len(args):
        a = args[i]
        if a in value_opts and i + 1 < len(args):
            opts[a] = args[i + 1]
            i += 2
        else:
            flags.add(a)
            i += 1
    return opts, flags


def find(items, service, account):
    for item in items:
        if item["service"] == service and item["account"] == account:
            return item
    return None


def not_found(out):
    out["err"].append(
        "security: SecKeychainSearchCopyNext: The specified item could not be found in the keychain."
    )
    return NOT_FOUND


def run(argv, out):
    if not argv:
        return 1
    cmd, rest = argv[0], argv[1:]
    fail_path = os.path.join(STATE_DIR, f"fail_{cmd}")
    if os.path.exists(fail_path):
        with open(fail_path) as f:
            rc = int(f.read().strip())
        out["err"].append("security: SecKeychainItemModifyContent: User interaction is not allowed.")
        return rc
    opts, flags = parse(cmd, rest)
    items = load()
    service, account = opts.get("-s"), opts.get("-a")
    if cmd == "find-generic-password":
        item = find(items, service, account)
        if item is None:
            return not_found(out)
        if "-w" in flags:
            out["out"].append(item["password"] + "\n")
        else:
            out["out"].append(
                f'keychain: "fake"\nclass: "genp"\nattributes:\n'
                f'    "acct"<blob>="{item["account"]}"\n'
                f'    "svce"<blob>="{item["service"]}"\n'
            )
        return 0
    if cmd == "add-generic-password":
        if "-X" in opts:
            password = bytes.fromhex(opts["-X"]).decode("utf-8")
        else:
            password = opts.get("-w", "")
        item = find(items, service, account)
        if item is not None and "-U" not in flags:
            out["err"].append(
                "security: SecKeychainItemCreateFromContent: The specified item already exists in the keychain."
            )
            return DUPLICATE
        if item is None:
            items.append({"service": service, "account": account, "password": password})
        else:
            item["password"] = password
        save(items)
        return 0
    if cmd == "delete-generic-password":
        item = find(items, service, account)
        if item is None:
            return not_found(out)
        items.remove(item)
        save(items)
        out["out"].append(f'keychain: "fake"\npassword has been deleted.\n')
        return 0
    out["err"].append(f"security: unknown command {cmd!r}")
    return 1


def main():
    stdin = sys.stdin.read() if sys.argv[1:2] == ["-i"] else None
    with open(CALLS, "a") as f:
        f.write(json.dumps({"argv": sys.argv[1:], "stdin": stdin}) + "\n")
    out = {"out": [], "err": []}
    if os.path.exists(FAIL_RC):
        with open(FAIL_RC) as f:
            rc = int(f.read().strip())
        sys.stderr.write("security: SecKeychainCopyDefault: User interaction is not allowed.\n")
        return rc
    if stdin is not None:
        rc = 0
        for line in stdin.splitlines():
            if line.strip():
                rc = run(shlex.split(line), out)
    else:
        rc = run(sys.argv[1:], out)
    sys.stdout.write("".join(out["out"]))
    if out["err"]:
        sys.stderr.write("\n".join(out["err"]) + "\n")
    return rc


sys.exit(main())

"""Run upstream `cswap` with the same seams the Rust binary exposes.

Upstream hard-codes /usr/bin/security and its HTTP endpoints, so the harness
launches it through this shim: it redirects both to the test doubles named by
the CCSWAP_* variables and refuses to start, or to touch the real store or
Keychain, when they are missing.
"""

import os
import pwd
import shutil
import sys
import urllib.error
import urllib.request

REQUIRED = (
    "CCSWAP_SECURITY_PATH",
    "CCSWAP_API_BASE_URL",
    "CCSWAP_OAUTH_TOKEN_URL",
    "CCSWAP_UPDATE_CHECK_URL",
)
missing = [name for name in REQUIRED if not os.environ.get(name)]
if missing:
    sys.exit(f"upstream_shim: refusing to start without {', '.join(missing)}")

REAL_HOME = os.path.realpath(pwd.getpwuid(os.getuid()).pw_dir)
if os.path.realpath(os.environ.get("HOME", "")) == REAL_HOME:
    sys.exit("upstream_shim: refusing to start with the real HOME")

PROTECTED = tuple(
    os.path.join(REAL_HOME, name)
    for name in (".claude-swap-backup", ".claude", ".claude.json", "Library/Keychains")
)
REAL_SECURITY = "/usr/bin/security"
REAL_LAUNCHCTL = "/bin/launchctl"
PATH_EVENTS = {
    "open", "os.mkdir", "os.rename", "os.replace", "os.remove", "os.rmdir",
    "os.chmod", "os.symlink", "os.link", "shutil.rmtree", "shutil.move",
}


def _is_protected(path) -> bool:
    if isinstance(path, int):
        return False
    if isinstance(path, bytes):
        path = os.fsdecode(path)
    full = os.path.realpath(os.fspath(path))
    return any(full == p or full.startswith(p + os.sep) for p in PROTECTED)


def _resolves_to(candidate, real) -> bool:
    name = os.fsdecode(candidate)
    found = name if os.sep in name else shutil.which(name)
    return bool(found) and os.path.realpath(found) == real


def _audit(event, args):
    if event in PATH_EVENTS:
        for arg in args[:2]:
            if isinstance(arg, (str, bytes, os.PathLike)) and _is_protected(arg):
                raise RuntimeError(f"upstream_shim: blocked {event} on real path {arg!r}")
    elif event == "subprocess.Popen":
        executable, argv = args[0], args[1]
        first = argv[0] if isinstance(argv, (list, tuple)) and argv else argv
        for candidate in (executable, first):
            if candidate and os.path.realpath(os.fsdecode(candidate)) == REAL_SECURITY:
                raise RuntimeError("upstream_shim: blocked spawn of the real security")
            if candidate and _resolves_to(candidate, REAL_LAUNCHCTL):
                raise RuntimeError("upstream_shim: blocked spawn of the real launchctl")


sys.addaudithook(_audit)

# Keep the macOS keyring library (and its in-process Keychain access) unreachable.
sys.modules["keyring"] = None

REWRITES = (
    ("https://api.anthropic.com", os.environ["CCSWAP_API_BASE_URL"]),
    ("https://platform.claude.com/v1/oauth/token", os.environ["CCSWAP_OAUTH_TOKEN_URL"]),
    ("https://pypi.org/pypi/claude-swap/json", os.environ["CCSWAP_UPDATE_CHECK_URL"]),
)
_real_urlopen = urllib.request.urlopen


def _urlopen(req, *args, **kwargs):
    url = req.full_url if isinstance(req, urllib.request.Request) else req
    for prefix, target in REWRITES:
        if url.startswith(prefix):
            new_url = target.rstrip("/") + url[len(prefix):]
            if isinstance(req, urllib.request.Request):
                req.full_url = new_url
            else:
                req = new_url
            return _real_urlopen(req, *args, **kwargs)
    raise urllib.error.URLError(f"upstream_shim: blocked request to {url}")


urllib.request.urlopen = _urlopen

sys.path.insert(0, os.environ["CCSWAP_UPSTREAM_SRC"])

from claude_swap import macos_keychain  # noqa: E402

macos_keychain._SECURITY = os.environ["CCSWAP_SECURITY_PATH"]

from claude_swap.cli import main  # noqa: E402

sys.argv[0] = "cswap"
main()

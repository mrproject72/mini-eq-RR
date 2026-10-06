#!/usr/bin/env python3
"""Call the mini-eq D-Bus API and print results as plain KEY=VALUE lines.

Usage:
  dbus.py getstate            -> KEY=VALUE per line (strings unquoted)
  dbus.py call <Method> [args...]  (args: b:true/false or s:text)
"""
import subprocess
import sys

BUS = "io.github.mrproject72.mini_eq_rr"
OBJ = "/io/github/mrproject72/mini_eq_rr/Control"
IFACE = "io.github.mrproject72.MiniEqRR.Control"


def call(method, args):
    wrapped = []
    for kind, val in args:
        if kind == "b":
            # gdbus booleans are passed bare, not wrapped in <>
            wrapped.append("true" if val == "true" else "false")
        else:
            # GVariant text for a string is "double-quoted"; <> would make
            # it a *variant* and the raw wrapper text leaked into the app
            # as the literal value (seen in the live-test app log).
            escaped = val.replace("\\", "\\\\").replace('"', '\\"')
            wrapped.append(f'"{escaped}"')
    cmd = ["gdbus", "call", "--session", "--dest", BUS,
           "--object-path", OBJ, "--method", f"{IFACE}.{method}"] + wrapped
    r = subprocess.run(cmd, capture_output=True, text=True, timeout=15)
    return r


def parse_state(text):
    """Parse gdbus a{sv} into a dict of raw value strings."""
    import re
    state = {}
    for m in re.finditer(r"'(\w+)': <(.*?)>(?=[,}]\s*['}])", text):
        state[m.group(1)] = m.group(2)
    # fallback: looser scan for nested arrays (capabilities etc.)
    return state


def main():
    if sys.argv[1] == "getstate":
        r = call("GetState", [])
        if r.returncode != 0:
            print(f"ERROR: {r.stderr.strip()}", file=sys.stderr)
            return 1
        import re
        text = r.stdout.strip()
        # entries look like 'key': <value> ; values may contain , inside []
        for m in re.finditer(r"'(\w+)': <((?:[^<>]|\[[^\]]*\])*)>", text):
            v = m.group(2).strip()
            if v.startswith("'") and v.endswith("'"):
                v = v[1:-1]
            print(f"{m.group(1)}={v}")
        return 0
    if sys.argv[1] == "call":
        method = sys.argv[2]
        args = []
        for a in sys.argv[3:]:
            if a.startswith("b:"):
                args.append(("b", a[2:]))
            elif a.startswith("s:"):
                args.append(("s", a[2:]))
            else:
                args.append(("s", a))
        r = call(method, args)
        if r.returncode != 0:
            print(f"ERROR: {r.stderr.strip()}", file=sys.stderr)
            return 1
        print(r.stdout.strip() or "OK")
        return 0
    print("usage: dbus.py getstate|call", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())

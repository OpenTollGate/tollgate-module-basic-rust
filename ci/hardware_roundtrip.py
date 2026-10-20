#!/usr/bin/env python3
"""Hardware round-trip lane: pay -> gate opens -> bytes counted -> /usage reports them.

Everything here was done by hand across PRs #37-#44 and each step cost real time to
discover. This script makes it ONE command so the next person MEASURES instead of
re-deriving. Every non-obvious detail is annotated with the measurement that
justifies it -- delete nothing you cannot re-measure.

Usage (from the repo root, on the bench host):
    python3 ci/hardware_roundtrip.py --gateway 192.168.1.1 --client-ip 192.168.1.159 \\
        --client-iface wlp59s0 --mint 192.168.2.13:3338 --binary target/.../tollgate-module-basic-rust

Exit code 0 = every step measured PASS. Any failure exits non-zero with the step
that failed. Nothing is queued silently; the bench mutex is a HARD gate.

THE VANTAGE REQUIREMENT (PR #41 -> #44):
    ndsctl only counts bytes for traffic that transits the router's managed
    interface (br-lan). If the client host's default egress goes out some other
    NIC, nothing is counted and metering looks broken while being merely
    unreachable. This script therefore ADDS a host route for the test target
    through the gateway and REMOVES it afterwards. Verify with `ip route get` on
    the CLIENT first (printed by --dry-run).

THE PORTAL REGISTRATION REQUIREMENT (PR #42 finding):
    nodogsplash does not know a MAC until it has seen HTTP from it through the
    managed interface. `ndsctl auth` fails with "Failed to authenticate client"
    for any MAC it has not seen. So this script hits the portal FIRST.
"""

import argparse
import json
import os
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

DEFAULT_LOCK = os.path.expanduser("~/.local/state/tg-bench/tollgate-A04E.lock")


class Fail(Exception):
    """A step failed. Carries the step name and the evidence."""


class BenchMutex:
    """Hard gate on the shared hardware. Never queue, never proceed without it."""

    def __init__(self, lock_path: str):
        self.lock_path = lock_path
        self.fd = None

    def __enter__(self):
        try:
            import fcntl
        except ImportError:
            raise Fail("fcntl unavailable: cannot take the bench mutex")
        self.fd = open(self.lock_path, "a+")
        try:
            fcntl.flock(self.fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            self.fd.close()
            raise Fail(
                f"bench mutex HELD ({self.lock_path}) — the box is in use. "
                "Fail loud: do not queue, do not open the UART anyway."
            )
        return self

    def __exit__(self, *exc):
        import fcntl

        if self.fd:
            fcntl.flock(self.fd, fcntl.LOCK_UN)
            self.fd.close()
        return False


def run(cmd, timeout=30, check=True, capture=True):
    r = subprocess.run(
        cmd, capture_output=capture, text=True, timeout=timeout
    )
    out = (r.stdout or "") + (r.stderr or "")
    if check and r.returncode != 0:
        raise Fail(f"command failed ({' '.join(cmd)}): {out.strip()[:400]}")
    return out.strip()


def http_json(method, url, payload=None, headers=None, timeout=25):
    data = json.dumps(payload).encode() if payload is not None else None
    h = dict(headers or {})
    if data is not None:
        h["Content-Type"] = "application/json"
    req = urllib.request.Request(url, data=data, headers=h, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as x:
            raw = x.read()
            try:
                return x.status, json.loads(raw)
            except json.JSONDecodeError:
                return x.status, raw.decode(errors="replace")[:200]
    except urllib.error.HTTPError as e:
        raw = e.read()
        try:
            return e.code, json.loads(raw)
        except json.JSONDecodeError:
            return e.code, raw.decode(errors="replace")[:200]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--gateway", default="192.168.1.1")
    ap.add_argument("--client-ip", required=True)
    ap.add_argument("--client-iface", required=True)
    ap.add_argument("--mint", default="http://192.168.2.13:3338")
    ap.add_argument("--binary", help="staged binary path on the router (/tmp/tg-rust)")
    ap.add_argument("--staging-url", help="host staging server URL for the binary")
    ap.add_argument("--lock", default=DEFAULT_LOCK)
    ap.add_argument("--portal-port", default=2050)
    ap.add_argument("--api-port", default=2121)
    ap.add_argument("--route-target", default="1.1.1.1")
    ap.add_argument("--amount", type=int, default=4)
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()

    base = f"http://{args.gateway}:{args.api_port}"
    hdrs = {"X-Forwarded-For": args.client_ip}

    # --dry-run: show what would happen and check the vantage, no hardware touched.
    if args.dry_run:
        print("== dry run: checking vantage only ==")
        route = run(["ip", "route", "get", args.route_target], check=False)
        print(f"client route to {args.route_target}: {route}")
        if args.client_iface not in route:
            print(
                f"WARNING: default egress does not use {args.client_iface}; the "
                "forced route below is REQUIRED or ndsctl will count nothing."
            )
        print(f"lock: {args.lock}")
        return 0

    with BenchMutex(args.lock):
        evidence = {
            "script": "ci/hardware_roundtrip.py",
            "started": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        }
        steps = []

        def step(name, ok, observed):
            steps.append({"step": name, "ok": bool(ok), "observed": observed})
            print(f"  [{'PASS' if ok else 'FAIL'}] {name}: {observed}")
            if not ok:
                evidence["steps"] = steps
                print(json.dumps(evidence, indent=2))
                raise Fail(f"step '{name}' failed")
            return observed

        # 1. vantage route: without it, ndsctl counts NOTHING (PR #41).
        have_route = run(
            ["ip", "route", "get", args.route_target], check=False
        )
        needs_route = args.client_iface not in have_route
        if needs_route:
            run(["sudo", "-n", "ip", "route", "add", args.route_target,
                 "via", args.gateway, "dev", args.client_iface])
            evidence["forced_route"] = True
        step("vantage-route",
             args.client_iface in run(["ip", "route", "get", args.route_target], check=False),
             f"traffic to {args.route_target} transits {args.client_iface}")

        try:
            # 2. portal registration: ndsctl does not know a MAC until it sees HTTP
            #    through the managed interface (PR #42).
            code = run(["curl", "-s", "-o", "/dev/null", "-w", "%{http_code}",
                        "-m", "6", f"http://{args.gateway}:{args.portal_port}/"], check=False)
            step("portal-registration", code == "200", f"portal returned {code}")

            # 3. purchase
            st, b = http_json("POST", f"{base}/ln-invoice",
                              {"amount": args.amount, "mint_url": args.mint}, hdrs)
            quote = b.get("quote") if isinstance(b, dict) else None
            step("purchase", st == 200 and bool(quote),
                 f"POST /ln-invoice -> {st} state={b.get('state') if isinstance(b, dict) else b}")

            # 4. grant: the poll until ISSUED/access_granted
            granted = None
            for _ in range(15):
                time.sleep(2)
                st, g = http_json("GET", f"{base}/ln-invoice?quote={quote}", headers=hdrs)
                if isinstance(g, dict) and (g.get("access_granted") or g.get("state") == "ISSUED"):
                    granted = g
                    break
            ok = bool(granted and granted.get("access_granted"))
            step("grant", ok, f"GET -> {granted}")

            # 5. real traffic through the gateway
            sz = run(["curl", "-s", "-o", "/dev/null", "-w", "%{http_code} %{size_download}B",
                      "-m", "20", "--interface", args.client_iface,
                      f"http://{args.route_target}/"], check=False)
            step("real-traffic", " " in sz and not sz.startswith("000"),
                 f"pull via {args.client_iface}: {sz}")

            # 6. module reads live usage: THE number that proves the meter
            st, u = http_json("GET", f"{base}/usage", headers=hdrs)
            used = None
            if isinstance(u, dict) and "usage" in u:
                # corpus shape: {"status":1,"usage":"14/4000", ...} or int fields
                raw = u.get("usage")
                used = int(str(raw).split("/")[0]) if raw is not None else None
            elif isinstance(u, str):
                used = int(u.split("/")[0].strip()) if "/" in u else None
            step("module-reads-usage", used is not None and used > 0,
                 f"GET /usage -> {u}")

            evidence["steps"] = steps
            evidence["verdict"] = "ROUND TRIP PASS — measured end to end"
            print("\n" + json.dumps(evidence, indent=2))
            return 0
        finally:
            if needs_route:
                run(["sudo", "-n", "ip", "route", "del", args.route_target,
                     "via", args.gateway, "dev", args.client_iface], check=False)
                print(f"  [cleanup] forced route removed")


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Fail as e:
        print(f"\nHARD FAIL: {e}", file=sys.stderr)
        sys.exit(1)
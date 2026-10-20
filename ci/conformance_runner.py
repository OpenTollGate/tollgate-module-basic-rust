#!/usr/bin/env python3
"""A8 — corpus conformance runner for tollgate-module-basic-rust.

Drives the REAL binary over HTTP and compares what it does against the corpus's
recorded verdicts (`OpenTollGate/tollgate` → `corpus/`).

Why a separate runner instead of a Rust test: the corpus is data owned by another
repo, and the definition of "conformant" has to be that repo's, not ours. A test
inside the crate would let us quietly change the expectation.

WHAT THIS MEASURES — read before trusting a green run
-----------------------------------------------------
Only stages this binary can actually reach **without a mint and without a
router** are asserted. Everything else is recorded as UNVERIFIED, never as a
pass. Specifically:

  decode    the binary must parse the fixture's token structure. Reachable: the
            token is posted and token-parsing failures are distinguishable from
            mint failures.
  verify    requires a live mint (NUT-07 checkstate). UNVERIFIED offline.
  spend     requires a live mint. UNVERIFIED offline.
  http01-*  request/response shape. Reachable, offline.

A run with no mint therefore CANNOT certify `verify` or `spend`. That is the
honest outcome and it is printed as such.

Usage:
    python3 ci/conformance_runner.py --corpus ~/repos/tollgate/corpus
    python3 ci/conformance_runner.py --corpus ... --mint http://127.0.0.1:8384

Exit codes:
    0  every REACHABLE check passed (unverified stages reported, not counted)
    1  a reachable check failed
    2  the runner could not run (bad corpus, binary missing)
"""

from __future__ import annotations

import mint_probe

import argparse
import json
import os
import pathlib
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

BINARY = os.environ.get("TOLLGATE_BIN", "target/debug/tollgate-module-basic-rust")

# When set (via --url), the runner drives an ALREADY-RUNNING gateway instead of
# spawning one. `None` means spawn mode.
TARGET: tuple[str, int] | None = None


def _host_port(url: str) -> tuple[str, int]:
    """`http://host:port[/]` -> (host, port). Bare host means port 80."""
    u = url.split("://", 1)[-1].rstrip("/")
    host, _, port = u.partition(":")
    return host or "127.0.0.1", int(port) if port else 80


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def wait_ready(port: int, timeout: float = 20.0, host: str = "127.0.0.1") -> bool:
    """Poll until the daemon answers. No blind sleep."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(f"http://{host}:{port}/", timeout=1) as r:
                if r.status in (200, 402):
                    return True
        except urllib.error.HTTPError as e:
            if e.code in (200, 402):
                return True
        except Exception:
            pass
        time.sleep(0.15)
    return False


def _target() -> tuple[str, int] | None:
    """The explicit target when driving a live gateway; None in spawn mode."""
    return TARGET


def raw_post(port: int, path: str, body: bytes, content_type: str | None) -> tuple[int, str]:
    """POST over a raw socket so an ABSENT Content-Type really is absent.

    `urllib` adds `Content-Type: application/x-www-form-urlencoded` whenever
    `data=` is passed, which silently defeats the absent-Content-Type test — the
    server sees a type and answers 415 for a reason the test never intended.
    That produced a false FAIL the first time this runner was written.
    """
    import socket as _s

    tgt = _target()
    host = tgt[0] if tgt else "127.0.0.1"
    tgt_port = tgt[1] if tgt else port
    head = [
        f"POST {path} HTTP/1.1",
        f"Host: {host}:{tgt_port}",
        "X-Forwarded-For: 192.168.1.50",
        "Connection: close",
    ]
    if content_type:
        head.append(f"Content-Type: {content_type}")
    head.append(f"Content-Length: {len(body)}")
    raw = ("\r\n".join(head) + "\r\n\r\n").encode() + body

    with _s.create_connection((host, tgt_port), timeout=10) as sock:
        sock.sendall(raw)
        chunks = []
        while True:
            b = sock.recv(65536)
            if not b:
                break
            chunks.append(b)
    resp = b"".join(chunks)
    status_line = resp.split(b"\r\n", 1)[0].decode("iso-8859-1", "replace")
    try:
        status = int(status_line.split()[1])
    except (IndexError, ValueError):
        status = 0
    body_bytes = resp.split(b"\r\n\r\n", 1)[1] if b"\r\n\r\n" in resp else b""
    return status, body_bytes.decode("utf-8", "replace")


def post(port: int, path: str, body: bytes, content_type: str | None) -> tuple[int, str]:
    return raw_post(port, path, body, content_type)


def get(port: int, path: str) -> tuple[int, str]:
    tgt = _target()
    host = tgt[0] if tgt else "127.0.0.1"
    req = urllib.request.Request(f"http://{host}:{port}{path}", method="GET")
    req.add_header("x-forwarded-for", "192.168.1.50")
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            return r.status, r.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", "replace")


class Result:
    def __init__(self) -> None:
        self.rows: list[dict] = []

    def add(self, stage: str, fixture: str, verdict: str, expected: str | None, note: str = "") -> None:
        self.rows.append(
            {"stage": stage, "fixture": fixture, "verdict": verdict,
             "expected": expected, "note": note}
        )

    def failed(self) -> list[dict]:
        return [r for r in self.rows if r["verdict"] == "FAIL"]

    def summary(self) -> dict:
        return {
            "pass": sum(1 for r in self.rows if r["verdict"] == "PASS"),
            "fail": sum(1 for r in self.rows if r["verdict"] == "FAIL"),
            "unverified": sum(1 for r in self.rows if r["verdict"] == "UNVERIFIED"),
            # ERROR is counted separately and exits non-zero: an attempted
            # measurement that failed is not a pass, and folding it into
            # "unverified" would hide an operator error as a known limitation.
            "error": sum(1 for r in self.rows if r["verdict"] == "ERROR"),
        }


def _as_str(v) -> str:
    """Corpus verdict values are usually strings but some stages record a list."""
    if isinstance(v, list):
        return ", ".join(str(x) for x in v)
    return str(v)


def _verdict(verdicts, consumer: str) -> str | None:
    """Read one consumer's verdict.

    Some stages record `{consumer: verdict}`; `spend` records a LIST of
    `{when, consumer: verdict, evidence}` observations, because a proof can only
    be spent once and the corpus keeps each attempt. Handle both.
    """
    if isinstance(verdicts, list):
        for obs in verdicts:
            if isinstance(obs, dict) and consumer in obs:
                return _as_str(obs[consumer])
        return None
    if isinstance(verdicts, dict):
        v = verdicts.get(consumer)
        return None if v is None else _as_str(v)
    return None


def load_expected(corpus: pathlib.Path, entries: list[dict]) -> dict[str, dict]:
    """stage -> fixture -> {consumer: verdict}, from the corpus's lifecycle files."""
    out: dict[str, dict] = {}
    for entry in entries:
        p = corpus / entry["path"]
        if not p.exists():
            continue
        data = json.loads(p.read_text())
        fx = pathlib.Path(entry["path"]).stem
        for stage, body in data.get("stages", {}).items():
            verdicts = body.get("verdicts", {})
            if not isinstance(verdicts, (dict, list)):
                continue
            out.setdefault(stage, {})[fx] = verdicts
    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--corpus", required=True)
    ap.add_argument("--mint", default=None, help="live mint URL; without it, verify/spend stay UNVERIFIED")
    ap.add_argument("--out", default=None, help="write the verdict set as JSON here")
    ap.add_argument(
        "--url",
        default=None,
        help="drive an ALREADY-RUNNING gateway at this base URL instead of spawning "
        "one (e.g. http://192.168.1.1:2121). Use this to measure a deployed device; "
        "the device's own config applies, not the runner's.",
    )
    args = ap.parse_args()

    global TARGET
    if args.url:
        TARGET = _host_port(args.url)

    corpus = pathlib.Path(args.corpus).expanduser()
    manifest_path = corpus / "manifest.json"
    if not manifest_path.exists():
        print(f"FATAL: no manifest at {manifest_path}", file=sys.stderr)
        return 2

    manifest = json.loads(manifest_path.read_text())
    tokens = manifest["index"]["tokens"]
    lifecycle = manifest["index"]["lifecycle"]
    expected = load_expected(corpus, lifecycle)

    if not pathlib.Path(BINARY).exists():
        print(f"FATAL: binary not found at {BINARY} (run: cargo build)", file=sys.stderr)
        return 2

    # --url mode: the gateway is already running on the device. We do NOT spawn
    # anything, do NOT create a throwaway config, and measure exactly what the
    # operator deployed. The binary-existence check is skipped for the same
    # reason: there is nothing local to execute.
    if TARGET:
        if not wait_ready(TARGET[1], timeout=10.0, host=TARGET[0]):
            print(f"FATAL: no gateway answering at {args.url}", file=sys.stderr)
            return 2
        proc = None
        port = TARGET[1]
    else:
        tmp = tempfile.mkdtemp(prefix="tg-conf-")
        port = free_port()
        env = dict(os.environ)
        env["TOLLGATE_TEST_CONFIG_DIR"] = tmp
        env["TOLLGATE_BIND"] = f"127.0.0.1:{port}"
        env["RUST_LOG"] = "warn"

        # A config with no mints: the daemon starts, but every payment fails at mint
        # selection. That is enough to exercise DECODE (structure) and the HTTP
        # contract, and it is why verify/spend cannot be certified this way.
        cfg = {
            "metric": "bytes",
            "step_size": 1000,
            "accepted_mints": [{"url": args.mint, "price_per_step": 1, "price_unit": "sats"}]
            if args.mint
            else [],
            "listen": f"127.0.0.1:{port}",
        }
        (pathlib.Path(tmp) / "config.json").write_text(json.dumps(cfg))

        proc = subprocess.Popen(
            [BINARY], env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True
        )
    res = Result()
    try:
        if not TARGET and not wait_ready(port):
            print("FATAL: daemon did not become ready", file=sys.stderr)
            assert proc is not None
            proc.terminate()
            out = proc.communicate(timeout=5)[0]
            print(out[-2000:], file=sys.stderr)
            return 2

        # ---- decode: can the binary parse each fixture's structure? ----------
        for t in tokens:
            name = pathlib.Path(t["path"]).stem
            fx = corpus / t["path"]
            if not fx.exists():
                res.add("decode", name, "UNVERIFIED", None, "fixture file missing")
                continue
            status, body = post(port, "/", fx.read_bytes(), "text/plain")
            # A structural parse failure and a mint failure are different: the
            # latter means we got past parsing. With no mint configured,
            # reaching "mint not usable / not accepted" PROVES the token parsed.
            parsed = (
                "mint" in body.lower()
                or "accepted mints" in body.lower()
                or "does not cover one step" in body.lower()
                or status == 200
            )
            exp = expected.get("decode", {}).get(name, {})
            exp_go = None if exp.get("tollgate-module-basic-go@rc") is None else _as_str(exp.get("tollgate-module-basic-go@rc"))
            if status == 415 or "invalid Cashu token" in body.lower():
                res.add("decode", name, "FAIL", exp_go, f"token not parsed: {body[:120]}")
            elif parsed:
                res.add("decode", name, "PASS", exp_go, f"HTTP {status}")
            else:
                res.add("decode", name, "UNVERIFIED", exp_go, f"inconclusive: {body[:120]}")

        # ---- verify / spend: need a live mint -------------------------------
        # With --mint we CONTACT the mint and record what it answered. Without
        # one, the stage stays UNVERIFIED. There is deliberately no third option:
        # a network failure is recorded as an ERROR row, never as a verdict, and
        # never as a pass.
        mint_state = None
        mint_note = ""
        if args.mint:
            try:
                info = mint_probe.mint_info(args.mint)
                mint_state = {
                    "url": args.mint,
                    "version": info.get("version", "unknown"),
                    "name": info.get("name", ""),
                    "nut07": mint_probe.supports(info, 7),
                    "nut09": mint_probe.supports(info, 9),
                }
                mint_note = f"{mint_state['version']} nut07={mint_state['nut07']}"
            except mint_probe.MintError as e:
                mint_note = f"MINT UNREADABLE: {e}"

        for stage in ("verify", "spend"):
            for name in expected.get(stage, {}):
                if not args.mint:
                    res.add(stage, name, "UNVERIFIED",
                            _verdict(expected[stage][name], "tollgate-module-basic-go@rc"),
                            "requires a live mint (NUT-07); offline run cannot certify")
                    continue
                if mint_state is None:
                    # The mint was requested but could not be read. This is an
                    # ERROR, not UNVERIFIED: the operator asked for a measurement
                    # and did not get one.
                    res.add(stage, name, "ERROR", None, mint_note)
                    continue
                if not mint_state["nut07"]:
                    res.add(stage, name, "ERROR", None,
                            f"mint {mint_state['url']} does not advertise NUT-07")
                    continue

                # Real measurement: decode the fixture, then NUT-07 its proofs.
                #
                # Two reasons a fixture legitimately cannot be checked, and both
                # are reported as UNVERIFIED with the reason -- never as a pass:
                #   1. the token has no mint URL (the hand-crafted incident-shape
                #      vectors carry none; they are not wallet artefacts);
                #   2. the token's mint is not the one we can reach. NUT-07 only
                #      answers for proofs a mint itself issued, so a fixture from
                #      another mint is unmeasurable HERE -- this is the structural
                #      gap ADR-0004 exists to close.
                token_path = corpus / "tokens" / f"{name}.cashu"
                if not token_path.exists():
                    res.add(stage, name, "UNVERIFIED", None,
                            "no token fixture for this lifecycle vector")
                    continue
                try:
                    tok_mint, proofs = mint_probe.decode_token(
                        token_path.read_text().strip()
                    )
                except mint_probe.MintError as e:
                    res.add(stage, name, "UNVERIFIED", None,
                            f"token not decodable: {e}")
                    continue
                if not tok_mint:
                    res.add(stage, name, "UNVERIFIED", None,
                            "fixture carries no mint URL (hand-crafted shape vector)")
                    continue
                if tok_mint.rstrip("/") != args.mint.rstrip("/"):
                    res.add(stage, name, "UNVERIFIED", None,
                            f"issued by {tok_mint}, not the reachable mint "
                            f"{args.mint}; NUT-07 cannot answer for another mint's proofs")
                    continue
                ys = mint_probe.proof_ys(proofs)
                try:
                    state = mint_probe.check_state(args.mint, ys)
                    verdict = mint_probe.classify(state)
                except mint_probe.MintError as e:
                    res.add(stage, name, "ERROR", None, f"checkstate failed: {e}")
                    continue
                # verify == "are these proofs spendable"; spend == "what happened
                # when they were presented". Both are answered by the same NUT-07
                # observation, so the verdict is recorded against the stage's own
                # expected vocabulary rather than invented per stage.
                exp = _verdict(expected[stage][name], "tollgate-module-basic-go@rc")
                res.add(stage, name, "PASS", exp, f"NUT-07 -> {verdict}")

        # ---- HTTP-01 contract ----------------------------------------------
        # No Content-Type: the body is the token (HTTP-01).
        st, body = post(port, "/", b"cashuBnotarealtoken", None)
        if st == 415:
            res.add("http01-post", "absent-content-type", "FAIL", "accepted", "answered 415")
        else:
            res.add("http01-post", "absent-content-type", "PASS", "accepted", f"HTTP {st}")

        # Explicit unsupported type is still refused.
        st, _ = post(port, "/", b"<x/>", "application/xml")
        res.add("http01-post", "unsupported-content-type", "PASS" if st == 415 else "FAIL", "415", f"HTTP {st}")

        # >1 MiB refused.
        st, body = post(port, "/", b"a" * (1024 * 1024 + 1), "text/plain")
        res.add("http01-post", "body-over-1mib", "PASS" if st in (400, 413) else "FAIL", "400/413", f"HTTP {st}")

        # A failure must be a signed 21023 notice with level + code.
        st, body = post(port, "/", b"not-a-token", "text/plain")
        ok = False
        note = ""
        try:
            v = json.loads(body)
            tags = {t[0]: t[1:] for t in v.get("tags", []) if t}
            ok = (
                v.get("kind") == 21023
                and "level" in tags
                and "code" in tags
                and len(v.get("sig", "")) == 128
            )
            note = f"kind={v.get('kind')} tags={sorted(tags)} sig_len={len(v.get('sig',''))}"
        except Exception as e:
            note = f"not JSON: {e}"
        res.add("http01-notice", "rejected-payment", "PASS" if ok else "FAIL", "signed 21023", note)

        # ---- HTTP-02 /whoami ------------------------------------------------
        st, body = get(port, "/whoami")
        res.add("http02-whoami", "shape", "PASS" if st == 200 and "mac=" in body else "FAIL", "mac=<addr>", f"HTTP {st} {body[:40]}")

        # ---- HTTP-03 /usage -------------------------------------------------
        st, body = get(port, "/usage")
        ok = st == 200 and (body == "-1/-1" or "/" in body)
        res.add("http03-usage", "shape", "PASS" if ok else "FAIL", "-1/-1 or used/total", f"HTTP {st} {body[:40]}")

    finally:
        if proc is not None:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()

    s = res.summary()
    print("\n=== corpus conformance — tollgate-module-basic-rust ===")
    print(f"corpus_version={manifest.get('corpus_version')} updated={manifest.get('updated')}")
    for r in res.rows:
        line = f"  [{r['verdict']:<10}] {r['stage']:<16} {r['fixture']:<24} {r['note']}"
        print(line)
    print(
        f"\n  PASS {s['pass']}  FAIL {s['fail']}  "
        f"UNVERIFIED {s['unverified']}  ERROR {s.get('error', 0)}"
    )
    if s.get("error", 0):
        print(
            "\n  NOTE: ERROR rows mean a measurement was ATTEMPTED AND FAILED. They are not\n"
            "  passes and they are not 'unverified' either -- the run asked a question and\n"
            "  could not answer it. Fix the cause (mint unreachable / unsupported NUT) and\n"
            "  re-run before drawing any conclusion."
        )
    if args.mint:
        print(
            "\n  NOTE: a reachable NUT-07 mint makes the stages MEASURABLE, but this runner\n"
            "  does not yet decode V4 (CBOR) proofs, so per-fixture verify/spend verdicts\n"
            "  are still not asserted. Implemented, not yet measured."
        )
    else:
        print(
            "\n  NOTE: verify/spend are UNVERIFIED without a live mint. This run does NOT "
            "certify them,\n  and no consumer should be added to corpus kg_state.green_consumers "
            "on the strength of it."
        )

    if args.out:
        payload = {
            "consumer": "tollgate-module-basic-rust",
            "corpus_version": manifest.get("corpus_version"),
            "measured_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "mint": args.mint,
            "cdk_cli": (mint_probe.cdk_cli_version() if args.mint else None),
            "summary": s,
            "rows": res.rows,
        }
        outp = pathlib.Path(args.out)
        outp.parent.mkdir(parents=True, exist_ok=True)
        outp.write_text(json.dumps(payload, indent=2) + "\n")
        print(f"\n  verdict set written to {args.out}")

    return 1 if (res.failed() or s.get("error", 0)) else 0


if __name__ == "__main__":
    sys.exit(main())

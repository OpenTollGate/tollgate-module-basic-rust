"""P2 -- live mint checks against a real Cashu mint.

Why this is a separate module from the runner: the runner's job is to *report*
verdicts honestly, and the one failure mode that would ruin it is a network error
being recorded as a pass. Isolating the mint contact here makes the rule enforceable
in one place: **anything we could not read is an error, never a verdict.**

The two rules that matter for correctness:

1. A NUT-07 checkstate must happen immediately before a spend. Cashu proofs are
   single-use; a stale "unspent" read is how you double-spend a fixture.
2. An unreadable spend state is NOT "unspent". Fail closed. A corpus twin marked
   UNSPENT is a known-good reference, and wrongly declaring it spent retires it
   from the corpus forever.

No `requests` dependency: stdlib only, so the runner stays runnable on a bare host.
"""

from __future__ import annotations

import base64
import json
import os
import re
import shutil
import subprocess
import urllib.error
import urllib.request

# Corpus verdict vocabulary (corpus/manifest.json -> vocabularies.verify / .spend).
# An answer outside this set is an ERROR, not a verdict -- see `classify`.
KNOWN_VERDICTS = frozenset(
    {
        "ok",
        "spent",
        "unknown",
        "refused-degraded-unconsumed",
        "refused",
        "error",
    }
)


class MintError(RuntimeError):
    """The mint could not be read. Never convert this into a verdict."""


def _post_json(url: str, payload: dict, timeout: float = 20.0) -> dict:
    data = json.dumps(payload).encode()
    req = urllib.request.Request(
        url,
        data=data,
        method="POST",
        headers={
            "Content-Type": "application/json",
            # Cloudflare (error 1010) rejects python-urllib's default UA. A mint is
            # a public HTTP service; identifying ourselves plainly is enough.
            "User-Agent": "tollgate-conformance-runner/1.0",
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.loads(r.read().decode())
    except urllib.error.HTTPError as e:
        detail = e.read().decode("utf-8", "replace")[:300]
        raise MintError(f"{url}: HTTP {e.code}: {detail}") from None
    except (urllib.error.URLError, TimeoutError, ValueError) as e:
        raise MintError(f"{url}: unreachable or unreadable: {e}") from None


def _get_json(url: str, timeout: float = 20.0) -> dict:
    try:
        # Cloudflare fronts several public mints and rejects python-urllib's
        # default User-Agent with HTTP 403 (error 1010). Identify plainly.
        req = urllib.request.Request(
            url, headers={"User-Agent": "tollgate-conformance-runner/1.0"}
        )
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.loads(r.read().decode())
    except urllib.error.HTTPError as e:
        detail = e.read().decode("utf-8", "replace")[:300]
        raise MintError(f"{url}: HTTP {e.code}: {detail}") from None
    except (urllib.error.URLError, TimeoutError, ValueError) as e:
        raise MintError(f"{url}: unreachable or unreadable: {e}") from None


def mint_info(mint_url: str) -> dict:
    """NUT-06 info. Raises MintError if the mint cannot be read."""
    return _get_json(mint_url.rstrip("/") + "/v1/info")


def supports(info: dict, nut: int) -> bool:
    return str(nut) in {str(k) for k in info.get("nuts", {})}


CDK_CLI_ENV = "CDK_CLI"


def _cdk_cli() -> str:
    """Path to `cdk-cli`, or raise MintError naming the missing tool.

    ADR-0003: V4 decoding is delegated to cdk-cli, the corpus's certified
    reference consumer. A conformance runner should agree with the implementation
    the corpus trusts, not with a decoder written by the party being measured.
    """
    candidates = []
    env = os.environ.get(CDK_CLI_ENV)
    if env:
        candidates.append(env)
    candidates += [
        os.path.expanduser("~/mint/cdk-cli"),
        "cdk-cli",
    ]
    for c in candidates:
        found = c if os.path.sep in c else shutil.which(c)
        if found and os.path.exists(found) and os.access(found, os.X_OK):
            return found
    raise MintError(
        f"cdk-cli not found (looked at ${CDK_CLI_ENV}, ~/mint/cdk-cli, PATH). "
        "V4/CBOR token decoding is delegated to it per ADR-0003; without it the "
        "proofs cannot be read and the stage is UNVERIFIED, not passed."
    )


def cdk_cli_version() -> str:
    """`cdk-cli --version`, for the verdict set's provenance."""
    cli = _cdk_cli()
    proc = subprocess.run(
        [cli, "--version"], capture_output=True, text=True, timeout=30
    )
    return (proc.stdout or proc.stderr).strip().splitlines()[0] if proc.returncode == 0 else "unknown"


def _hex_or_str(v):
    """cdk-cli prints byte fields as `h'..'`; normalise to a hex string."""
    if not isinstance(v, str):
        return v
    if v.startswith("h'") and v.endswith("'"):
        return v[2:-1]
    return v


def decode_token(token: str):
    """Return (mint_url, proofs) for a Cashu token.

    `cashuA` is base64 JSON and decoded inline. `cashuB` is CBOR and delegated to
    `cdk-cli` (ADR-0003) — never guessed, never silently empty.
    """
    token = token.strip()
    if token.startswith("cashuA"):
        body = token[len("cashuA") :]
        body += "=" * (-len(body) % 4)
        d = json.loads(base64.urlsafe_b64decode(body))
        # Two V3 shapes exist in the wild:
        #   flat     {"m": url, "proofs": [...]}                 (older / hand-built)
        #   grouped  {"token": [{"mint": url, "proofs": [...]}], "unit": ...}  (cdk-cli)
        # Reading only the flat shape silently returns zero proofs for cdk-cli
        # output, which then looks like "nothing to check" rather than a parse bug.
        mint = d.get("m")
        proofs = d.get("proofs")
        if mint is None and isinstance(d.get("token"), list) and d["token"]:
            g = d["token"][0]
            mint = g.get("mint") or g.get("m")
            proofs = g.get("proofs") or g.get("p")
        if proofs is None:
            t = d.get("t")
            if isinstance(t, dict):
                mint = mint or t.get("m")
                proofs = t.get("p")
        proofs = proofs or []
        return mint, proofs

    if token.startswith("cashuB"):
        cli = _cdk_cli()
        proc = subprocess.run(
            [cli, "decode-token", token], capture_output=True, text=True, timeout=60
        )
        if proc.returncode != 0:
            raise MintError(
                "cdk-cli could not decode the V4 token: "
                f"{(proc.stderr or proc.stdout).strip()[:300]}"
            )
        # cdk-cli pretty-prints with Rust Debug formatting, which is NOT JSON:
        # byte fields appear as h'01e7..' and objects carry TRAILING COMMAS. Both
        # must be normalised before parsing. Recorded here because assuming
        # cdk-cli emits strict JSON is the bug that produced this comment.
        # cdk-cli also prints status chatter before the payload, e.g.
        # "Recovered 3 operations, 0 compensated, ...". Slice from the first brace
        # so we parse the object and not the narration around it.
        out = proc.stdout
        brace = out.find("{")
        if brace < 0:
            raise MintError(f"cdk-cli printed no JSON object: {out.strip()[:200]}")
        out = out[brace:]
        raw = re.sub(r"h'([0-9a-fA-F]*)'", r'"\1"', out)
        raw = re.sub(r",(\s*[}\]])", r"\1", raw)
        try:
            d = json.loads(raw)
        except ValueError as e:
            raise MintError(
                f"cdk-cli output was not JSON even after normalising h'..' "
                f"literals and trailing commas: {e}"
            ) from None

        mint = d.get("m")
        if not mint:
            raise MintError("cdk-cli decode returned no mint URL")
        proofs = []
        for g in d.get("t") or []:
            for pr in g.get("p") or []:
                proofs.append(
                    {
                        "amount": pr.get("a"),
                        "secret": _hex_or_str(pr.get("s")),
                        "C": _hex_or_str(pr.get("c")),
                    }
                )
        if not proofs:
            # A decoded token with no proofs is malformed, not an empty stage.
            # Returning silence here would read as "nothing to check".
            raise MintError("cdk-cli decoded the V4 token but found no proofs")
        return mint, proofs

    raise MintError(f"unrecognised token format: {token[:12]!r}")


def check_state(mint_url: str, ys: list[str]) -> dict[str, str]:
    """NUT-07 checkstate for a list of proof secrets.

    Returns {Y: state}. Raises MintError if the mint did not answer -- a caller
    must not treat that as "unspent".
    """
    url = mint_url.rstrip("/") + "/v1/checkstate"
    resp = _post_json(url, {"Ys": ys})
    states = resp.get("states")
    if not isinstance(states, list):
        raise MintError(f"checkstate: no 'states' in response: {str(resp)[:200]}")
    out = {}
    for entry in states:
        y = entry.get("Y")
        st = entry.get("state")
        if y is None or st is None:
            raise MintError(f"checkstate: malformed entry {entry!r}")
        out[y] = st
    return out


def classify(checkstate: dict[str, str]) -> str:
    """Map a NUT-07 answer onto the corpus vocabulary.

    ALL_UNSPENT -> ok (proofs are spendable: a verification pass)
    anything SPENT -> spent
    a mixed/unknown state -> error, because the corpus vocabulary has no honest
    word for "I do not know" and inventing one is how a runner starts lying.
    """
    if not checkstate:
        raise MintError("checkstate returned no proofs")
    states = {s.upper() for s in checkstate.values()}
    if states == {"UNSPENT"}:
        return "ok"
    if "SPENT" in states:
        return "spent"
    if states <= {"UNSPENT", "PENDING"}:
        # PENDING is transient; not a certified pass.
        raise MintError(f"proofs in a transient state: {sorted(states)}")
    raise MintError(f"unrecognised checkstate vocabulary: {sorted(states)}")


def verdict_is_known(v: str) -> bool:
    return v in KNOWN_VERDICTS


def proof_ys(proofs) -> list[str]:
    """Y values for a NUT-07 checkstate.

    NUT-07 takes `Ys` = compressed secp256k1 *points*, not secrets. Posting
    secrets returns HTTP 422 and would make every row look like a mint error.
    """
    import nut00

    secrets = []
    for p in proofs:
        sec = p["secret"] if isinstance(p, dict) else p
        if not sec:
            raise MintError("proof without a secret cannot be checked")
        secrets.append(sec)
    return nut00.proof_ys(secrets)

"""NUT-00 hash_to_curve: secret -> Y (a compressed secp256k1 point).

Why this exists: NUT-07 `checkstate` takes `Ys` — *public keys*, not secrets. Posting
secrets returns HTTP 422. Getting this wrong would silently make every verify/spend
row an ERROR, which looks like a mint problem rather than a runner bug.

Pure stdlib on purpose (see ADR-0003: no CBOR/crypto dependency for the runner). The
only primitive needed is decompressing `02 || x`, which for secp256k1 is a modular
square root: p ≡ 3 (mod 4), so y = pow(x³ + 7, (p+1)/4, p).

NUT-00 hash_to_curve:
    msg = secret bytes
    loop:
        h = sha256(msg)
        try: return 02 || h   (i.e. h is a valid x-coordinate)
        except: msg = h        (try-and-increment)
"""

from __future__ import annotations

import hashlib

P = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEFFFFFC2F


def _decompress(x: int) -> int | None:
    """Return y for `02 || x`, or None if x is not on the curve."""
    if x >= P:
        return None
    alpha = (pow(x, 3, P) + 7) % P
    y = pow(alpha, (P + 1) // 4, P)
    if (y * y) % P != alpha:
        return None
    # NUT-00 uses the even y (compressed prefix 02).
    if y & 1:
        y = P - y
    return y


def hash_to_curve(secret: str | bytes) -> str:
    """Y as 33-byte compressed hex (`02||x`), as NUT-07 expects."""
    msg = secret.encode() if isinstance(secret, str) else secret
    for _ in range(1000):
        h = hashlib.sha256(msg).digest()
        x = int.from_bytes(h, "big")
        if _decompress(x) is not None:
            return "02" + h.hex()
        msg = h
    raise RuntimeError("hash_to_curve: no valid x found in 1000 attempts")


def proof_ys(secrets: list[str]) -> list[str]:
    return [hash_to_curve(s) for s in secrets]

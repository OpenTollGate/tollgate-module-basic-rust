# ADR-0001: Durable state — SQLite, WAL, `synchronous=FULL` for money

- **Status:** Accepted
- **Date:** 2026-10-18
- **Supersedes:** the in-memory `HashMap` behaviour described in the original README

## Context

`SessionManager` and `MintQuoteRegistry` were in-memory. A restart lost every session
and every unpaid quote.

Two different kinds of loss, only one of which is a convenience problem:

- **Session lost** → a customer who paid is cut off mid-allotment. Their token is
  already spent at the mint. This is a money loss with no recovery path.
- **Quote lost** → a paid invoice can never be redeemed. Also a money loss.
- **Usage counter lost** → the customer gets their allotment again. A small,
  bounded give-away. Covered separately by ADR-0002.

Persistence was therefore not a reliability nicety but a correctness requirement.

## Decision

1. **SQLite**, via `rusqlite`, in one file: `${TOLLGATE_STATE_DB}` defaulting to
   `<db_dir>/tollgate.db` — the same volume as the wallet, so one backup or one
   drained volume covers both.
2. **`journal_mode=WAL`, `synchronous=FULL`.** Non-negotiable for the tables that
   carry money (sessions, quotes).
3. **Explicit columns, no serialised blobs**, with `PRAGMA user_version` and a
   refusal to start on a version this build does not know.
4. **Fail closed.** An unreadable or corrupt store must never be read as "no
   sessions". The binary exits **78 (EX_CONFIG)** naming the path.

## Why SQLite and not a journal file

`cdk-sqlite` is already a direct dependency, so `rusqlite` is already compiled and
linked into the binary. A second storage engine for two tables is unjustified — and a
hand-rolled journal would need its own crash-recovery logic, which is exactly the code
you do not want to write for the thing storing paid access.

## Why `FULL` and not `NORMAL`

Under WAL, `synchronous=NORMAL` can lose the most recent transactions on power loss.
Routers lose power; that is their normal failure mode, not an edge case. Losing the
last session write after the customer's token was already spent means taking money
and giving nothing.

**Cost:** one fsync per session create/revoke/quote transition. A few milliseconds,
at a request rate where a payment happens every few minutes. Measured acceptable.

## Why fail closed

Starting with an empty store would silently grant fresh access to every client — that
is strictly worse than being down, because nobody notices and nothing is logged.
Refusing to start is loud, and the operator can fix the store.

A guard against the subtle version of this bug: **absence is an error, not a zero.**
The same rule applies to `ndsctl` metering (see `src/metering/`), where `used = 0` for
an unknown client is indistinguishable from a client that transferred nothing.

## Consequences

- Recovery is explicit, not incidental:
  - sessions whose expiry passed while down are **closed**, not revived — a restart
    must not extend access;
  - still-valid sessions reload with `used` **intact** — resetting it would hand out
    free bytes on every restart;
  - exhausted sessions do not return active;
  - unpaid quotes past deadline are marked expired, never deleted; paid ones stay
    redeemable;
  - revocation is persisted, so a revoked session cannot come back.
- Backup is one file (plus its `-wal`/`-shm` siblings while running).
- Tests read `PRAGMA journal_mode` and `PRAGMA synchronous` **back from the
  database**, not from the source, so a silent regression to `NORMAL` fails CI.

## Revisit if

- Write latency on real router flash becomes a measured problem (not a guessed one),
  in which case the answer is batching per ADR-0002, not weakening `FULL` for money.
- SQLite is removed from the dependency tree for another reason, at which point the
  "already linked" argument disappears and the choice should be re-made.

# Architecture Decision Records

Decisions that are expensive to reverse, or that a future contributor would
otherwise re-litigate. Each ADR states the decision, the reason, and — importantly —
what it costs and what would make us revisit it.

An ADR here is **binding on this repository** until superseded. Superseding means a
new ADR with `Supersedes: ADR-000N`, not an edit to the old one; the history of the
reasoning is the point.

| ADR | Title | Status |
|---|---|---|
| [0001](0001-durable-state-wal-full.md) | Durable state: SQLite, WAL, `synchronous=FULL` for money | Accepted |
| [0002](0002-batched-usage-writes.md) | Batched writes for the usage counter | Accepted |
| [0003](0003-v4-cbor-via-cdk-cli.md) | Decode V4 (CBOR) tokens via `cdk-cli`, not in-process | Accepted |
| [0004](0004-corpus-fixtures-from-testnut.md) | Corpus fixtures come from a reachable mint, regenerable by script | Accepted |
| [0005](0005-licence-undecided.md) | Licence remains undecided | Open — deliberately |
| [0006](0006-gate-close-and-durability.md) | Close the gate, and keep it open across restarts | Accepted — measured (#49, #50) |
| [0007](0007-identity-is-the-socket.md) | Identity is the socket, not a header a caller can write | Accepted — implemented (#54) |

## Writing one

- One decision per ADR. If you need "and", it is two ADRs.
- Record the **cost** as well as the benefit. An ADR that only lists advantages is
  advocacy, not a record.
- State the **revisit trigger** explicitly. "Never" is an acceptable answer but must
  be written down.

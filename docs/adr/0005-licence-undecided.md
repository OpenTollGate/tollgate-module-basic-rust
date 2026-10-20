# ADR-0005: Licence remains undecided

- **Status:** Open — deliberately
- **Date:** 2026-10-18
- **Tracking:** PR #13 (proposal), still open

## Context

The situation, stated plainly:

- `tollgate-module-basic-go` is **GPL-3.0**.
- This repository declares **MIT** in `Cargo.toml` but ships **no `LICENSE` file**.
  A declaration in a manifest without the licence text is not a licence grant.
- This repository is a reimplementation, not a port: no Go code was copied (see the
  Path A working agreement), which is why declaring MIT was not obviously wrong.
- But the two implementations solve the same protocol problem and their code could
  converge. If they ever share code, a GPL/MIT mismatch makes that impossible
  permanently.

## Decision

**Do not decide yet. Do not create a `LICENSE` file.**

This is an operator/legal decision, not an engineering one, and making it implicitly
by committing a licence file would be worse than leaving it open — a licence is
expensive to retract once downstream users rely on it.

The open question is recorded in PR #13 with specific questions to the maintainers, so
it is tracked and cannot be quietly forgotten.

## What is explicitly NOT decided

- Whether this repository should be MIT, GPL-3.0, or something else.
- Whether cross-implementation code sharing will ever be wanted. ANSWER THIS FIRST —
  if the answer is "yes", the licence must match GPL-3.0.
- Whether "declared MIT in `Cargo.toml`, no `LICENSE` file" is a state the project is
  willing to ship. Currently it is, and it should be treated as a known gap.

## Constraint on contributors while this is open

- **Do not copy code from `tollgate-module-basic-go`.** It is GPL-3.0 and this
  repository declares MIT. Reimplement from the spec and the corpus.
- **Do not create a `LICENSE` file.** That would settle the question by accident.

## Recommendation on record

If forced to choose today: **GPL-3.0-match**, on the reasoning that these are daemons
deployed on routers rather than libraries others link against, so MIT buys little,
while a licence mismatch permanently forecloses code sharing between the two
implementations of one protocol.

That is a recommendation, not a decision. It stays open until the maintainers answer.

## Revisit if

- The maintainers answer in PR #13.
- Someone wants to share code between the Go and Rust implementations — that forces
  the answer immediately.
- A downstream consumer needs a clear licence before adopting, which makes "undecided"
  a blocker rather than a deferral.

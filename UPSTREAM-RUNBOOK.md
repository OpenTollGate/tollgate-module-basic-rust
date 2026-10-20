# Upstream consolidation runbook (prepared 2026-10-20)

PRs are BLOCKED ONLY by upstream being empty. Once OpenTollGate/tollgate-module-basic-rust
has ANY commit on main (even a README), run:

    gh pr create --repo OpenTollGate/tollgate-module-basic-rust       --head felixfelix-bot:pr/import-1-core --base main       --title "import: core module — contract surface, wallet, sessions, gate lifecycle"       --body-file /tmp/pr1body.md
    gh pr create --repo OpenTollGate/tollgate-module-basic-rust       --head felixfelix-bot:pr/import-2-conformance --base pr/import-1-core       --title "import: conformance runner + hardware-measured verdict sets"       --body-file /tmp/pr2body.md
    gh pr create --repo OpenTollGate/tollgate-module-basic-rust       --head felixfelix-bot:pr/import-3-adrs --base pr/import-2-conformance       --title "import: decision records — ADR-0001..0007"       --body-file /tmp/pr3body.md

Branches already pushed to felixfelix-bot/tollgate-module-basic-rust:
  pr/import-1-core          (71 files)
  pr/import-2-conformance   (81 files, stacked)
  pr/import-3-adrs          (92 files == full fork main tree, verified identical)

Note: PR 2/3 bases reference the fork's branches; GitHub allows same-network stacked
bases only if the base branch exists UPSTREAM. If cross-repo stacking is refused,
merge PR 1 first, then rebase 2 and 3 onto upstream/main and re-target.

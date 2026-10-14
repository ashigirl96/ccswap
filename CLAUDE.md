## Local install

After a PR merges or a commit lands on `main`, pull `main` and run `just install`. The user's menu bar and desktop widget run from `~/Applications/ccswap.app`, so merged code reaches them only through this. Done when `ccswap menubar --service-status` reports `running`.

## Agent skills

### Issue tracker

Issues live in GitHub Issues of `ashigirl96/ccswap`, operated via `gh`. See `docs/agents/issue-tracker.md`.

### Triage labels

The five default labels: `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: one `GLOSSARY.md` and `docs/adr/` at the repo root. See `docs/agents/domain.md`.

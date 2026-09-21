## Agent skills

### Fork boundary (hard rule — every GitHub write)

Every `gh` write — issues, PRs, comments, labels, edits, closes — targets
`ki3lich/vod2pod-rss` and passes `-R ki3lich/vod2pod-rss` explicitly, since
`gh` defaults to the git remote, which points upstream. Nothing is ever
written to `madiele/vod2pod-rss` (upstream); reading upstream is fine.

### Issue tracker

Issues live in this repo's GitHub Issues (via the `gh` CLI). See `docs/agents/issue-tracker.md`.

### Triage labels

Five canonical roles, each label string equal to its role name (`needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`). See `docs/agents/triage-labels.md`.

### Domain docs

Single-context — one root `CONTEXT.md` + `docs/adr/`. See `docs/agents/domain.md`.

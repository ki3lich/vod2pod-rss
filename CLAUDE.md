## Agent skills

### Issue tracker

Issues live in this repo's GitHub Issues (via the `gh` CLI). See `docs/agents/issue-tracker.md`.

**Fork boundary (hard rule)**: never create issues or PRs, comment, label, push,
or perform any other write on `madiele/vod2pod-rss` (upstream). All GitHub
writes go exclusively to `ki3lich/vod2pod-rss` — pass `-R ki3lich/vod2pod-rss`
explicitly on every `gh` call, since `gh` defaults to the git remote, which
points upstream. Reading upstream is fine.

### Triage labels

Five canonical roles, each label string equal to its role name (`needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`). See `docs/agents/triage-labels.md`.

### Domain docs

Single-context — one root `CONTEXT.md` + `docs/adr/`. See `docs/agents/domain.md`.

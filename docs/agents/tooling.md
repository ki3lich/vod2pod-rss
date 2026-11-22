# Agent tooling

## Renovate

Verify a config or Dockerfile-tag change against the working tree. Install once
per session, then loop:

```sh
npm i -g renovate@43
LOG_LEVEL=debug renovate --platform=local --dry-run=lookup \
  --enabled-managers=custom.regex,dockerfile --require-config=optional > /tmp/rn.log
grep -n -A20 '"regex": \[' /tmp/rn.log
```

`--dry-run=extract` lists what Renovate detects; `lookup` adds the registry call.
`LOG_LEVEL` is an env var, `--log-level` is rejected. Read every dep's
`skipReason`: `invalid-value` means the tag is not version-first, so it never
gets a PR.

Keep the `latest-rust-<version>-` shape in the builder tag, and make
`versioningTemplate` and `extractVersionTemplate` normalize that same shape:
Docker versioning ranks `1.100` above `1.100.0`, so a three-part pin only wins
when the versioning coerces both. `.renovaterc.json5` pairs `semver-coerced`
with `^latest-rust-(?<version>\d+(?:\.\d+)*)-[a-z]+$`.

## pi-lens

`.pi-lens.json` scopes the JSON rules (`json:519`, `json:528`) off `*.json5`,
which holds JSON5 here (unquoted keys, trailing commas) rather than JSON. The
global config tier rejects `rules`, so a repo with JSON5 configs needs this file
of its own.

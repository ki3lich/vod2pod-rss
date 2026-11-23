# Agent tooling

## Renovate

Verify a config or Dockerfile-tag change against the working tree. `npm i -g` is
blocked in the sandbox, so run the CLI through `npx`:

```sh
LOG_LEVEL=debug npx --yes --package renovate@43 renovate --platform=local \
  --dry-run=lookup --enabled-managers=<MANAGERS> --require-config=optional > /tmp/rn.log
```

`<MANAGERS>` is `custom.regex,dockerfile` for a Dockerfile tag and `cargo` for a
Cargo dep. `--dry-run=extract` lists what Renovate detects; `lookup` adds the
registry call. `LOG_LEVEL` is an env var, `--log-level` is rejected. For a
Dockerfile tag, `grep -n -A20 '"regex": \[' /tmp/rn.log` shows the extracted dep;
read every dep's `skipReason`: `invalid-value` means the tag is not
version-first, so it never gets a PR.

## Cargo lookup probes

An empty `"updates": []` for the dep you just filtered proves nothing: the run
may have had no proposal to suppress. Probe the rule itself instead — filter a
dep the run does propose (`matchPackageNames: ["url"]` with
`allowedVersions: "<=2.5.8"`, while crates.io serves 2.5.9), rerun, and watch
that proposal disappear. The probe goes in `.renovaterc.json5` itself;
`git checkout -- .renovaterc.json5` puts it back.

## Docker tag versioning

Keep the `latest-rust-<version>-` shape in the builder tag, and make
`versioningTemplate` and `extractVersionTemplate` normalize that same shape:
Docker versioning ranks `1.100` above `1.100.0`, so a three-part pin only wins
when the versioning coerces both. `.renovaterc.json5` pairs `semver-coerced`
with `^latest-rust-(?<version>\d+(?:\.\d+)*)-[a-z]+$`.

## CI logs

A failed job's failure summary sits past the point where the tool's streamed
output truncates, so write the log to a file once and grep it:

```sh
gh run view --job <job-id> -R ki3lich/vod2pod-rss --log > /tmp/build.log
grep -n "failures:" -A 30 /tmp/build.log
```

`--log-failed` returns that same whole log when the failing step is the job
itself.

## pi-lens

`.pi-lens.json` scopes the JSON rules (`json:519`, `json:528`) off `*.json5`,
which holds JSON5 here (unquoted keys, trailing commas) rather than JSON. The
global config tier rejects `rules`, so a repo with JSON5 configs needs this file
of its own.

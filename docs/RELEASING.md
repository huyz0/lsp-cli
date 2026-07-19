# Releasing

## Cutting a release

```bash
git tag v0.2.0
git push origin v0.2.0
```

Pushing a `v*.*.*` tag triggers [`.github/workflows/release.yml`](../.github/workflows/release.yml):
builds `lsp` for Linux (x86_64/aarch64), macOS (x86_64/aarch64, i.e. Intel
and Apple Silicon), and Windows (x86_64), publishes a GitHub Release with
each archive plus a `checksums.txt`, then (if the secrets below are
configured) updates the Homebrew tap and submits the winget manifest.

`workflow_dispatch` with a `tag` input re-runs the same pipeline against an
existing tag, for retrying a failed publish step without cutting a new
version.

## One-time setup for package-manager publishing

Both of these are optional — the release itself (GitHub Release with
binaries) always works. Each publishing job checks for its secret and
skips quietly if it's missing, so an unconfigured one never fails the
release.

### Homebrew (`huyz0/homebrew-tap`)

The tap repo ([huyz0/homebrew-tap](https://github.com/huyz0/homebrew-tap))
already exists with a placeholder `Formula/lsp-cli.rb`. To let releases
update it automatically:

1. Create a PAT (classic, `repo` scope, or fine-grained with
   `contents:write` on `huyz0/homebrew-tap` only) at
   <https://github.com/settings/tokens>.
2. Add it as a repo secret here named `HOMEBREW_TAP_TOKEN`
   (Settings → Secrets and variables → Actions).

Once set, `brew install huyz0/tap/lsp-cli` picks up whatever the latest
release published.

### winget

1. Create a fine-grained PAT scoped per
   <https://github.com/vedantmgoyal9/winget-releaser#token> (needs to be
   able to open PRs against `microsoft/winget-pkgs`).
2. Add it as a repo secret named `WINGET_TOKEN`.

The **first** submission additionally goes through Microsoft's manual
manifest validation before `winget-pkgs` accepts it — that review happens
outside this repo and can take a few days; subsequent releases update the
same manifest automatically once the initial one is merged.

### No package manager / can't install a tap

[`install.sh`](../install.sh) downloads the right release asset directly:

```bash
curl -fsSL https://raw.githubusercontent.com/huyz0/lsp-cli-rust/main/install.sh | sh
```

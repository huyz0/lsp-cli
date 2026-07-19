# Releasing

## Cutting a release

```bash
git tag v0.2.0
git push origin v0.2.0
```

Pushing a `v*.*.*` tag triggers [`.github/workflows/release.yml`](../.github/workflows/release.yml):
builds `lsp` for Linux (x86_64/aarch64) and macOS (x86_64/aarch64, i.e.
Intel and Apple Silicon), publishes a GitHub Release with each archive plus
a `checksums.txt`, then (if the secret below is configured) updates the
Homebrew tap.

No Windows build: `src/main.rs` has a deliberate `compile_error!` for
non-Unix targets. The daemon (used by every navigation command for warm
server reuse) is Unix Domain Socket-only today, with no Windows named-pipe
transport implemented yet. A winget job (using
[vedantmgoyal9/winget-releaser](https://github.com/vedantmgoyal9/winget-releaser),
the same action [rtk-ai/rtk](https://github.com/rtk-ai/rtk) uses) makes
sense to add once that exists. See git history for the one that was
removed here after the first real CI run proved Windows doesn't compile.

`workflow_dispatch` with a `tag` input re-runs the same pipeline against an
existing tag, for retrying a failed publish step without cutting a new
version.

## One-time setup for Homebrew publishing

Optional. The release itself (GitHub Release with binaries) always
works. The publishing job checks for its secret and skips quietly if it's
missing, so leaving this unconfigured never fails a release.

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

### No package manager / can't install a tap

[`install.sh`](../install.sh) downloads the right release asset directly:

```bash
curl -fsSL https://raw.githubusercontent.com/huyz0/lsp-cli/main/install.sh | sh
```

# Cutting a release

Every release is a Git tag. The `release` workflow (`.github/workflows/release.yml`) builds that tag, writes the notes with `git-cliff` and publishes the GitHub Release. Nothing bumps the version for you, so a release is always: bump, merge, tag.

All crates inherit one version from `[workspace.package]` in the root `Cargo.toml`. The workflow checks the tag against it.

## Choosing the version

`git-cliff` groups the changelog from PR titles (see [writing-prs.md](writing-prs.md)), but you choose the number:

- **Patch** (`0.1.0` → `0.1.1`): only `fix`, `perf`, `docs` or `chore` changes.
- **Minor** (`0.1.1` → `0.2.0`): at least one `feat`.
- **`!` title or `BREAKING CHANGE:` footer**: below 1.0.0 this bumps the **minor** (`0.1.0` → `0.2.0`), never to `1.0.0`. From 1.0.0 on it bumps the major (`1.x` → `2.0.0`).
- **1.0.0** is a deliberate decision by the maintainers, not something a commit title can trigger.

`git cliff --bumped-version` prints a suggestion from the commits since the last tag.

## Release from `main`

1. Open a PR that sets `version` in `[workspace.package]` and updates `Cargo.lock`. Title it `chore(release): prepare vX.Y.Z`.
2. After it merges, tag the merge commit and push the tag:

   ```bash
   git switch main && git pull
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```

3. The tag push builds the release and creates it as a **draft**. Check the notes and binaries, then publish it from the Releases page.

The workflow refuses a tag whose commit has no successful `tests_pass` run, so wait for CI on the merge commit before tagging.

## What users download

Each release carries archives per target, a Windows setup wizard (`mudcrab-vX.Y.Z-windows-x64-setup.exe`), `SHA256SUMS.txt` and `release-metadata.json`.

- **Windows, interactive**: the setup wizard (Start Menu entries, uninstaller; per-user install, no admin rights needed). This is the recommended path.
- **Windows, scripted**: `scripts/install.ps1` unpacks the zip into `%LOCALAPPDATA%\Mudcrab\bin` and creates shortcuts. It does not touch `PATH`, because the binaries have generic names (`engine.exe`, `converter.exe`), and it has no uninstaller.
- **Linux and macOS**: `scripts/install.sh` installs into `~/.local/share/mudcrab` and links the binaries into `~/.local/bin` as `mudcrab-launcher`, `mudcrab-engine`, `mudcrab-converter` and `mudcrab-world-inspect`. The real files keep their plain names because the launcher looks for `engine` next to itself.

The launcher writes its converted output to `modern_assets` in its working directory, so the shortcuts set one: the install folder (`~/.local/share/mudcrab`) for the Linux menu entry, `%LOCALAPPDATA%\\Mudcrab` for the Windows wizard and script. Started from a terminal, it uses whatever folder you are in.

Both installer scripts verify the archive against `SHA256SUMS.txt` and stop if it cannot be fetched or does not match. `latest` resolves to the newest non-prerelease release; set `MUDCRAB_VERSION` to pick another tag.

> **Note on macOS builds**: macOS binaries are currently distributed unsigned and unnotarized until Apple Developer code-signing certificates are provisioned. macOS users may need to clear the quarantine flag (`xattr -d com.apple.quarantine <binary>`) or allow the application in System Settings > Privacy & Security.

> **Note on Windows builds**: the Windows setup wizard and binaries are unsigned, so SmartScreen may warn on first run ("More info" > "Run anyway").

Pushing the tag never publishes anything by itself: the workflow only ever creates a **draft** from a tag push, and nothing is public until someone presses Publish on the Releases page. The draft is where you inspect the notes and the binaries.

Run the `release` workflow by hand (**Actions → release → Run workflow**) only to build an existing tag again, for example after a runner failure, or to publish straight away with `draft` off. The tag must already exist, because the workflow builds that tag's commit, not the branch you run it from. The `skip_ci_gate` option bypasses the `tests_pass` check; use it only in a fork or for a dry run.

A tag with a suffix, such as `v0.2.0-rc.1`, needs `0.2.0-rc.1` as the Cargo version. It is published as a prerelease, so the installers don't pick it up as the latest release.

## Hotfix an older release

Use a release branch only when `main` has already moved on and you must patch an older line.

1. Create `release/vX.Y.x` from the last tag of that line, for example `git switch -c release/v0.1.x v0.1.0`, and push it. The `x` is literal; one branch serves every `v0.1.*` release.
2. Land the fix on it through a PR that targets the release branch.
   - If the bug also exists on `main`, fix it on `main` first, then **cherry-pick** that commit onto the release branch (`git cherry-pick <sha>`). This copies only that fix, not the rest of `main`.
   - If the bug only exists in the old release, fix it directly on the release branch.
3. Bump the version to `X.Y.(Z+1)`, merge, then tag and push as above, from the release branch.

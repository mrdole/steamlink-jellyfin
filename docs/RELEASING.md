# Branches, merging and releases

## Versions

- One version for the whole project, in `VERSION` (SemVer). `daemon/Cargo.toml` carries the same number; the
  daemon, player (`sljf-play --version`), tools and the Jellyfin device info all report it, and `deploy.sh` leaves
  it on the Link (`/mnt/scratch/sljf/VERSION`). `scripts/check.sh` fails if they disagree.
- While young: `0.MINOR.PATCH` with pre-release suffixes. `-alpha.N` = may break config or layout;
  `-beta.N` = feature-complete for that minor; `-rc.N` = release candidate. Without a suffix = stable for that minor.
- Bump MINOR for new features or config changes, PATCH for fixes only. Breaking changes are allowed in `0.x` but must be
  called out in the changelog under **Changed** with what to do.
- Releases are git tags `vX.Y.Z[-pre]` on `main`, with a GitHub release (pre-release for suffixed versions).

## Branches and merging

- `main` is protected: changes land through pull requests, the `check` CI job must pass, history stays linear
  (squash merge; the PR title becomes the commit message).
- Branch names: `feat/...`, `fix/...`, `docs/...`, `chore/...`. One topic per branch; rebase on `main` rather than
  merging `main` in.
- Every PR adds a line to `CHANGELOG.md` under `## [Unreleased]` (Added / Changed / Fixed / Removed), or carries the
  `skip-changelog` label for changes users won't notice.
- CI cannot build for the Link (the SDK is 4.7 GB): build locally with `scripts/build.sh` and say in the PR how it
  was tested on hardware.

```bash
git switch -c fix/short-name
# change, then: scripts/check.sh && scripts/build.sh
git push -u origin fix/short-name
gh pr create --fill
gh pr merge --squash --delete-branch      # after CI is green
```

## Making a release

```bash
git switch -c chore/release-0.1.0-alpha.2
scripts/bump-version 0.1.0-alpha.2        # VERSION, Cargo.toml/lock, CHANGELOG section from [Unreleased]
git commit -am "Release 0.1.0-alpha.2"
gh pr create --fill --label skip-changelog && gh pr merge --squash --delete-branch   # once CI is green
git switch main && git pull --ff-only
scripts/release.sh --dry-run              # shows the tag and notes
scripts/release.sh                        # tags vX.Y.Z..., pushes the tag, creates the GitHub release
```

## Downstream copies (private configs)

Keep your own settings (host alias, server, HA files) outside this repository and build from a release tag:
`git -C steamlink-jellyfin checkout v0.1.0-alpha.1 && scripts/build.sh && scripts/deploy.sh`. Fixes go upstream as
PRs here, not into a private copy.

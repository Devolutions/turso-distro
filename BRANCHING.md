# Branching and tagging strategy

## Goals

- Keep downstream automation and packaging separate from the upstream Turso source tree.
- Carry Devolutions source patches on explicit per-release branches based on upstream Turso release tags.
- Keep upstream and downstream refs distinct so workflow pins are auditable.
- Expose the patched source on `master` as a pinned submodule so workflows build the exact reviewed tree without a second checkout.
- Keep the Devolutions package identity out of the source branches: `master` re-vendors the upstream `Turso.*` packages as `Devolutions.Turso.*` after packing.

## Branches

- `master`
  - Downstream distribution-only branch.
  - Contains README, agent instructions, GitHub Actions workflows, helper scripts, and the package re-vendoring logic.
  - Do not add the upstream Turso source tree here.
- `upstream`
  - Mirror of the upstream `tursodatabase/turso` default branch (`main`).
  - Used for traceability and local comparisons, not for downstream patches.
- `downstream/vX.Y.Z`
  - Downstream patch branch for one upstream Turso release, for example `downstream/v0.9.0-pre.1`.
  - Created from the namespaced upstream tag `upstream/vX.Y.Z`.
  - Contains the full Turso source tree plus downstream patch commits.
  - Does not rename packages; package identity is handled by `eng/Vendor-TursoPackages.ps1` on `master`.

## Tags

- Upstream mirrored tags:
  - `upstream/vX.Y.Z`
  - Examples: `upstream/v0.8.3`, `upstream/v0.9.0-pre.1`
- Downstream release tags:
  - `v<upstream-version>.R`
  - `<upstream-version>` matches the upstream Turso version, including any prerelease label.
  - `R` is the downstream revision for additional releases from the same upstream version.
  - Example sequence for upstream `v0.9.0-pre.1`: `v0.9.0-pre.1.0`, `v0.9.0-pre.1.1`.
  - Example sequence for a stable upstream `v0.9.0`: `v0.9.0.0`, `v0.9.0.1`.

Do not use plain `vX.Y.Z` downstream tags. Those names belong to upstream Turso and are mirrored only under `upstream/*`.

The NuGet package version is the release tag without `v`. For prereleases the revision lands in the prerelease label (`0.9.0-pre.1.1`), so NuGet orders downstream revisions after the upstream prerelease and before the next upstream prerelease or the stable release.

## Release flow

1. Sync the upstream mirror branch and upstream tags (`.github/workflows/sync-turso-upstream.yml`, or locally):

   ```powershell
   .\scripts\Sync-TursoUpstream.ps1 -SyncTags
   ```

2. Create a patch branch from the upstream release tag without switching the downstream `master` worktree:

   ```powershell
   .\scripts\New-TursoPatchBranch.ps1 -Version 0.9.0-pre.1
   ```

3. Add a linked source worktree for `downstream/v0.9.0-pre.1` and apply downstream Turso source patches there (cherry-pick or rebase the previous downstream branch's patch commits).
4. Bump the `turso-src` submodule on `master` to the updated `downstream/vX.Y.Z` tip and update workflow pins so `TURSO_SOURCE_REF` points to the desired `downstream/vX.Y.Z` branch while `TURSO_RELEASE_TAG` remains the upstream Turso release tag.
5. Run `.github/workflows/turso-dotnet.yml` to build, re-vendor, and validate the packages, then `.github/workflows/turso-release.yml` to publish to Artifactory.
6. The release workflow tags downstream releases as `v<upstream-version>.R` when publishing.

## Workflow pin model

Turso workflows keep source checkout refs separate from release metadata:

- `TURSO_VERSION`: upstream version, for example `0.9.0-pre.1`. Must match `turso-src/bindings/dotnet/Directory.Build.props`.
- `TURSO_RELEASE_TAG`: upstream release tag, for example `v0.9.0-pre.1`.
- `TURSO_UPSTREAM_TAG`: mirrored upstream source ref used for ancestry checks and release patch export, for example `upstream/v0.9.0-pre.1`.
- `TURSO_SOURCE_REPOSITORY`: repository containing the source branch, normally this repository.
- `TURSO_SOURCE_REF`: downstream source branch to build, for example `downstream/v0.9.0-pre.1`.
- `PACKAGE_REVISION`: downstream revision `R`; reset to `0` for the first package built from a new upstream tag.

## Source submodule

`master` exposes the patched Turso source tree as a same-repository git submodule at path `turso-src/`:

```ini
[submodule "turso-src"]
    path = turso-src
    url = ./
    branch = downstream/v0.9.0-pre.1
```

The submodule pins a specific commit on `downstream/vX.Y.Z`, not the branch tip. Bumping the pointer is a normal commit on `master` and is reviewable in the PR diff. Workflows check out the project with `submodules: true`, which is enough to populate `turso-src/` using the default `GITHUB_TOKEN`; no second `actions/checkout` and no extra secrets are needed. The preflight job fails if the pinned commit is not on `TURSO_SOURCE_REF` or is not based on `TURSO_UPSTREAM_TAG`.

After rebasing `downstream/vX.Y.Z` (for example with an AI-assisted rebase that force-pushes the patch branch), re-bump the submodule on `master` so the pinned commit matches the new tip:

```powershell
git submodule update --remote turso-src
git add turso-src
```

## Local source worktree

The `turso-src/` submodule on `master` is the build source of truth, but it is a detached checkout of the pinned commit. For developing patches, use a linked worktree on the patch branch:

```powershell
git worktree add turso-src-worktree downstream/v0.9.0-pre.1
```

`turso-src-worktree/` is ignored by this repository. Commit source patches in that worktree on the `downstream/vX.Y.Z` branch, not on `master`, then bump the submodule pointer on `master` as described above.

## Patch export

To export a squashed patch for review or archival:

```powershell
.\scripts\Export-TursoPatch.ps1 -RepoPath turso-src-worktree -Branch downstream/v0.9.0-pre.1
```

The script compares `upstream/v0.9.0-pre.1...downstream/v0.9.0-pre.1` and writes the patch under `patchsets/`, which is ignored by git. The release workflow exports the same diff for the pinned submodule commit and attaches it to the GitHub release.

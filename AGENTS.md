# Agent instructions

This repository is a small GitHub Actions project for building Devolutions distributions of the Turso .NET packages. Treat `.github/workflows/turso-dotnet.yml` as the primary product surface: it builds the native `turso_sdk_kit` library for every platform, packs the upstream `Turso.*` NuGet packages from `turso-src/bindings/dotnet`, re-vendors them as `Devolutions.Turso.*`, and validates them. Packages are intentionally not code signed in this repository; binaries are signed when they ship in Devolutions products. `.github/workflows/turso-release.yml` publishes the validated packages to Artifactory and creates the GitHub release.

Keep version pins explicit in workflow `env` blocks. The version bump is four coordinated edits in one PR: the `TURSO_*` env vars (and `PACKAGE_REVISION`, reset to `0` for a new upstream tag) in `.github/workflows/turso-dotnet.yml`, the `branch = downstream/vX.Y.Z` line in `.gitmodules` (git does not expand variables there, so it must be edited literally), the `turso-src` submodule pointer on `master` (`git submodule update --remote turso-src && git add turso-src`), and the "Current pins" table in README.md. `TURSO_VERSION` must equal `<Version>` in `turso-src/bindings/dotnet/Directory.Build.props`; the pack job fails otherwise.

The repository uses a downstream patch branch model:

- `master` is downstream-only and must not contain the upstream Turso source tree directly. The patched source is exposed as a same-repo git submodule at path `turso-src/` (`url = ./`, `branch = downstream/vX.Y.Z`) pinned to a commit on the patch branch.
- `upstream` mirrors `tursodatabase/turso` `main` for traceability.
- `upstream/vX.Y.Z` mirrors upstream Turso release tags.
- `downstream/vX.Y.Z` branches contain full Turso sources plus downstream patches.
- Downstream release tags use `v<upstream-version>.R`, for example `v0.9.0-pre.1.0`.

Do not rename packages, change `PackageId`, or add Devolutions branding on `downstream/*` branches. Package identity is applied after `make pack` by `eng/Vendor-TursoPackages.ps1`, which rewrites the nuspec ID, version, dependencies, owners, repository metadata, `build`/`buildTransitive` imports named after the package ID, package ID references inside those MSBuild files, and package install snippets in the package README. Assembly names, namespaces, and native library names stay upstream. When upstream adds a package, add its output path to the pack job's "Collect upstream NuGet packages" step; the vendor script fails if a vendored package depends on a `Turso.*` package that is missing from the set.

In workflows, keep the source checkout ref separate from build version metadata. Workflows check out the project with `actions/checkout` using `submodules: true` to populate `turso-src/` from the pinned submodule commit; do not add a second `actions/checkout` for Turso source. When the patch branch is rebased and force-pushed, re-bump the `turso-src` submodule pointer on `master` with `git submodule update --remote turso-src` and commit that change.

Distro workflow file names (`turso-dotnet.yml`, `turso-release.yml`, `sync-turso-upstream.yml`) intentionally differ from upstream Turso workflow paths. The `upstream` branch, `upstream/*` tags, and `downstream/*` branches carry upstream's own `.github/workflows`; upstream's tag-triggered workflows (`release.yml`, `publish-crates.yml`, `aristo.yml`) can start when one to three new `upstream/*` tags are pushed, and upstream `pull_request` workflows run on PRs that target `downstream/*`. Those can be disabled by path in the Actions UI without affecting the distro workflows. Do not edit upstream workflow files on `downstream/*` branches to work around this.

`turso-src/` is a tracked submodule, not a generated directory. Do not commit generated `output/`, package, or build output directories, and do not commit a locally generated `turso-src-worktree/` worktree. Prefer validating workflow edits with a YAML parser and `git diff --check` before finishing.

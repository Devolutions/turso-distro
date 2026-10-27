# turso-distro

GitHub Actions workflows for building Devolutions distributions of the [Turso](https://github.com/tursodatabase/turso) .NET packages from upstream or downstream-patched source. The upstream `Turso.*` NuGet packages are built from source, re-vendored as `Devolutions.Turso.*`, validated, and published to the Devolutions Artifactory NuGet feed.

## Quick start

This repository builds Turso from source. The full Turso source tree is pulled in as a git submodule at `turso-src/`, so the checkout must initialize submodules, and on Windows long-path support should be enabled in git.

```powershell
git clone https://github.com/Devolutions/turso-distro.git
cd turso-distro
.\scripts\Initialize-Repository.ps1
```

All workflows are manual and start from the GitHub Actions **Run workflow** button (see the Workflows table below). The workflows are the authoritative release builds.

## Current pins

| Component | Version |
| --- | --- |
| Turso upstream release | `0.9.0-pre.1` / `v0.9.0-pre.1` |
| Turso downstream source ref | `downstream/v0.9.0-pre.1` based on `upstream/v0.9.0-pre.1` |
| Turso .NET target frameworks | `net8.0;net9.0;net10.0` |
| Downstream package version | `0.9.0-pre.1.0` workflow default |
| Package ID prefix | `Devolutions.Turso.` |
| Package feed | `https://devolutions.jfrog.io/artifactory/` repository `nuget` |

The `downstream/v0.9.0-pre.1` source branch carries these downstream patches on top of upstream `v0.9.0-pre.1`:

- .NET page codec API: `TursoPageCodec` in `Turso.Raw`, `TursoConnection.PageCodec`, and page codec support in the Sqlite facade and EF Core provider.
- Opt-in binary GUID binding: the `BinaryGUID` connection option stores `Guid` parameters as 16-byte blobs while `DbType.Guid` stays `TEXT`.

## Packages

| Upstream package | Devolutions package |
| --- | --- |
| `Turso.Data.Native` | `Devolutions.Turso.Data.Native` |
| `Turso.Data.Common` | `Devolutions.Turso.Data.Common` |
| `Turso.Data.Sqlite.Provider` | `Devolutions.Turso.Data.Sqlite.Provider` |
| `Turso.EntityFrameworkCore.Sqlite` | `Devolutions.Turso.EntityFrameworkCore.Sqlite` |
| `Turso.Data.NativeAot.<rid>` | `Devolutions.Turso.Data.NativeAot.<rid>` for `win-x64`, `win-arm64`, `linux-x64`, `linux-arm64`, `osx-x64`, `osx-arm64` |
| `Turso.Platform.Client` | `Devolutions.Turso.Platform.Client` |
| `Turso.Serverless.Client` | `Devolutions.Turso.Serverless.Client` |

Only package identity changes. Assembly names (`Turso.Data.dll`, `Turso.Raw.dll`, ...), namespaces, and the native library name (`turso_sdk_kit`) are unchanged, so switching from the upstream packages only requires changing the `PackageReference`:

```xml
<PackageReference Include="Devolutions.Turso.Data.Sqlite.Provider" Version="0.9.0-pre.1.0" />
```

With NuGet package source mapping, map `Devolutions.Turso.*` to the Artifactory feed.

## Workflows

| Workflow | Purpose | Output |
| --- | --- | --- |
| `.github/workflows/turso-dotnet.yml` | Builds the native `turso_sdk_kit` library for Windows, Linux, macOS, Android, and iOS, packs the upstream `Turso.*` packages with `make pack`, re-vendors them with `eng/Vendor-TursoPackages.ps1`, and validates the vendored packages with the upstream NativeAOT, Android, and iOS package consumer samples. It can run manually or as a reusable workflow. | `Devolutions-Turso-NuGet-<version>` artifact containing the vendored `.nupkg` files. |
| `.github/workflows/turso-release.yml` | Runs the build workflow, publishes the validated packages to Artifactory, and creates the GitHub release. | Artifactory publish plus GitHub release `v<version>` with the packages, the downstream Turso patch, and checksums. |
| `.github/workflows/sync-turso-upstream.yml` | Mirrors `tursodatabase/turso` `main` to `upstream` and release tags `v0.8.0` and later to `upstream/*`. | Updated `upstream` branch and `upstream/*` tags. |

## Publishing releases

The release version is `TURSO_VERSION.PACKAGE_REVISION`, such as `0.9.0-pre.1.0`. Manual inputs:

| Input | Purpose |
| --- | --- |
| `package_revision` | Optional revision override. Leave blank to use the `PACKAGE_REVISION` env pin, set a number for a specific downstream revision, or set `auto` to infer the next revision from existing `v<version>.R` tags. |
| `github-env` | `auto`, `test`, or `prod`. `auto` selects `publish-prod` for manual runs from `master` and `publish-test` elsewhere. |
| `skip-publish` | Builds and validates without publishing to Artifactory or creating a GitHub release. |
| `dry-run` | Simulates publishing. Defaults to `true`; non-production environments are forced to dry-run. |

Publishing requires the `ARTIFACTORY_USERNAME` and `ARTIFACTORY_PASSWORD` secrets on the `publish-prod` environment.

Packages are not code signed here; the binaries are signed when they ship inside Devolutions products such as RDM.

The upstream sync workflow needs a `TURSO_UPSTREAM_SYNC_TOKEN` or `DEVOLUTIONSBOT_WRITE_TOKEN` secret with `workflow` scope, because upstream refs contain workflow files.

## Re-vendoring packages locally

`eng/Vendor-TursoPackages.ps1` works on any folder of upstream Turso packages, for example the output of `make pack`:

```powershell
.\eng\Vendor-TursoPackages.ps1 -InputDirectory .\output\upstream-packages -OutputDirectory .\output\nuget -PackageVersion 0.9.0-pre.1.0 -UpstreamVersion 0.9.0-pre.1
```

## Updating Turso versions

When moving to a new upstream Turso release, the bump touches four surfaces in the same PR:

1. Workflow `env` block in `.github/workflows/turso-dotnet.yml`: `TURSO_VERSION`, `TURSO_RELEASE_TAG`, `TURSO_UPSTREAM_TAG`, and `TURSO_SOURCE_REF`. Reset `PACKAGE_REVISION` to `0`.
2. `.gitmodules`: the `branch = downstream/vX.Y.Z` line under `[submodule "turso-src"]`.
3. The `turso-src` submodule pointer on `master`, bumped to the new `downstream/vX.Y.Z` tip with `git submodule update --remote turso-src && git add turso-src`.
4. The "Current pins" table above.

This repository follows the downstream patch branch model of `Devolutions/pwsh-distro`. See [BRANCHING.md](BRANCHING.md) for the full branch, tag, submodule, and worktree flow.

Turso is MIT licensed; see `turso-src/LICENSE.md`.

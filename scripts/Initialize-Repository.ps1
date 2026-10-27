[CmdletBinding(SupportsShouldProcess)]
param(
  [string] $SubmodulePath = 'turso-src',

  [ValidateSet('Local', 'Global', 'Both', 'None')]
  [string] $LongPathScope = 'Both'
)

Set-StrictMode -Version 3.0
$ErrorActionPreference = 'Stop'

function Invoke-Git {
  param(
    [Parameter(Mandatory, ValueFromRemainingArguments)]
    [string[]] $Arguments
  )

  & git @Arguments
  if ($LASTEXITCODE -ne 0) {
    throw "git $($Arguments -join ' ') failed with exit code $LASTEXITCODE"
  }
}

function Get-GitOutput {
  param(
    [Parameter(Mandatory, ValueFromRemainingArguments)]
    [string[]] $Arguments
  )

  $Output = & git @Arguments
  if ($LASTEXITCODE -ne 0) {
    throw "git $($Arguments -join ' ') failed with exit code $LASTEXITCODE"
  }

  return $Output
}

function Test-WindowsPlatform {
  return [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
    [System.Runtime.InteropServices.OSPlatform]::Windows)
}

function Set-GitLongPaths {
  param(
    [Parameter(Mandatory)]
    [ValidateSet('Local', 'Global', 'Both')]
    [string] $Scope
  )

  $Scopes = if ($Scope -eq 'Both') { @('Local', 'Global') } else { @($Scope) }
  foreach ($ConfigScope in $Scopes) {
    $Arguments = if ($ConfigScope -eq 'Global') {
      @('config', '--global', 'core.longpaths', 'true')
    } else {
      @('config', 'core.longpaths', 'true')
    }

    if ($PSCmdlet.ShouldProcess("$ConfigScope git config", 'Set core.longpaths=true')) {
      Invoke-Git @Arguments
    }
  }
}

if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
  throw 'git was not found on PATH.'
}

$RepositoryRoot = (Get-GitOutput rev-parse --show-toplevel | Select-Object -First 1).Trim()
if (-not $RepositoryRoot) {
  throw 'Unable to determine git repository root.'
}

$PreviousLocation = Get-Location
try {
  Set-Location $RepositoryRoot

  if (-not (Test-Path '.gitmodules' -PathType Leaf)) {
    throw "No .gitmodules file was found in $RepositoryRoot."
  }

  if (Test-WindowsPlatform) {
    if ($LongPathScope -ne 'None') {
      Set-GitLongPaths -Scope $LongPathScope
    }

    $LongPaths = (& git config --bool --get core.longpaths 2>$null)
    if ($LASTEXITCODE -ne 0 -or $LongPaths -ne 'true') {
      throw "core.longpaths is not enabled for this repository. Re-run with -LongPathScope Local, Global, or Both."
    }
  }

  if ($PSCmdlet.ShouldProcess($SubmodulePath, 'Synchronize submodule URL configuration')) {
    Invoke-Git submodule sync --recursive -- $SubmodulePath
  }

  if ($PSCmdlet.ShouldProcess($SubmodulePath, 'Initialize and update submodule to the pinned commit')) {
    Invoke-Git submodule update --init --recursive -- $SubmodulePath
  }

  $SubmoduleStatus = @(Get-GitOutput submodule status --recursive -- $SubmodulePath)
  foreach ($Line in $SubmoduleStatus) {
    if ($Line.StartsWith('-')) {
      throw "Submodule is not initialized: $Line"
    }
    if ($Line.StartsWith('+')) {
      throw "Submodule checkout does not match the pinned commit: $Line"
    }
    if ($Line.StartsWith('U')) {
      throw "Submodule has unresolved conflicts: $Line"
    }
  }

  $BuildPropsPath = Join-Path $SubmodulePath 'bindings/dotnet/Directory.Build.props'
  if (-not (Test-Path $BuildPropsPath -PathType Leaf)) {
    throw "Turso source submodule did not initialize correctly; missing $BuildPropsPath."
  }

  [xml] $BuildProps = Get-Content -LiteralPath $BuildPropsPath
  $PropertyGroups = @($BuildProps.Project.PropertyGroup)
  $DotNetBindingVersion = $PropertyGroups |
    ForEach-Object { $_.Version } |
    Where-Object { $_ } |
    Select-Object -First 1
  $TargetFrameworks = $PropertyGroups |
    ForEach-Object { $_.TursoTargetFrameworks } |
    Where-Object { $_ } |
    Select-Object -First 1

  $SubmoduleCommit = (Get-GitOutput @('-C', $SubmodulePath, 'rev-parse', 'HEAD') | Select-Object -First 1).Trim()

  Write-Output "RepositoryRoot=$RepositoryRoot"
  if (Test-WindowsPlatform) {
    Write-Output "GitCoreLongPaths=$((& git config --bool --get core.longpaths).Trim())"
  }
  Write-Output "SubmodulePath=$SubmodulePath"
  Write-Output "SubmoduleCommit=$SubmoduleCommit"
  Write-Output "TursoDotNetBindingVersion=$DotNetBindingVersion"
  Write-Output "TursoTargetFrameworks=$TargetFrameworks"
} finally {
  Set-Location $PreviousLocation
}

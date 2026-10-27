[CmdletBinding()]
param(
  [Parameter(Mandatory)]
  [string] $InputDirectory,

  [Parameter(Mandatory)]
  [string] $OutputDirectory,

  [Parameter(Mandatory)]
  [string] $PackageVersion,

  [string] $OriginalIdPrefix = 'Turso.',

  [string] $VendorIdPrefix = 'Devolutions.Turso.',

  [string] $VendorName = 'Devolutions',

  [string] $RepositoryUrl = 'https://github.com/Devolutions/turso-distro',

  [string] $RepositoryCommit,

  [string] $UpstreamVersion
)

# Re-vendors upstream Turso .NET packages under Devolutions package IDs without
# touching the downstream source branches. Assembly names, namespaces, and
# native library names are kept; only package identity, version, dependency
# references, and package-ID-named MSBuild imports are rewritten.

Set-StrictMode -Version 3.0
$ErrorActionPreference = 'Stop'

Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

function Get-NormalizedNuGetVersion {
  param(
    [Parameter(Mandatory)]
    [string] $Version
  )

  if ($Version -notmatch '^(\d+)\.(\d+)\.(\d+)(?:\.(\d+))?(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?$') {
    throw "Invalid NuGet package version '$Version'."
  }

  $Parts = @([int]$Matches[1], [int]$Matches[2], [int]$Matches[3])
  if ($Matches[4] -and [int]$Matches[4] -ne 0) {
    $Parts += [int]$Matches[4]
  }

  $Normalized = $Parts -join '.'
  if ($Matches[5]) {
    $Normalized += "-$($Matches[5])"
  }

  return $Normalized
}

function Read-ZipEntryBytes {
  param(
    [Parameter(Mandatory)]
    [System.IO.Compression.ZipArchiveEntry] $Entry
  )

  $Stream = $Entry.Open()
  try {
    $Buffer = [System.IO.MemoryStream]::new()
    $Stream.CopyTo($Buffer)
    return ,$Buffer.ToArray()
  } finally {
    $Stream.Dispose()
  }
}

function Read-ZipEntryText {
  param(
    [Parameter(Mandatory)]
    [System.IO.Compression.ZipArchiveEntry] $Entry
  )

  $Bytes = Read-ZipEntryBytes $Entry
  return [System.Text.Encoding]::UTF8.GetString($Bytes).TrimStart([char]0xFEFF)
}

function Get-NuspecEntry {
  param(
    [Parameter(Mandatory)]
    [System.IO.Compression.ZipArchive] $Archive,

    [Parameter(Mandatory)]
    [string] $PackagePath
  )

  $Entries = @($Archive.Entries | Where-Object { $_.FullName -notmatch '/' -and $_.Name -like '*.nuspec' })
  if ($Entries.Count -ne 1) {
    throw "Expected exactly one root nuspec in '$PackagePath', found $($Entries.Count)."
  }

  return $Entries[0]
}

function New-NuspecNamespaceManager {
  param(
    [Parameter(Mandatory)]
    [xml] $Document
  )

  $NamespaceManager = [System.Xml.XmlNamespaceManager]::new($Document.NameTable)
  $NamespaceManager.AddNamespace('n', $Document.DocumentElement.NamespaceURI)
  return ,$NamespaceManager
}

function Get-NuspecMetadataValue {
  param(
    [Parameter(Mandatory)]
    [xml] $Document,

    [Parameter(Mandatory)]
    [string] $Name
  )

  $NamespaceManager = New-NuspecNamespaceManager $Document
  $Element = $Document.SelectSingleNode("/n:package/n:metadata/n:$Name", $NamespaceManager)
  if (-not $Element) {
    return $null
  }

  return $Element.InnerText
}

function Set-NuspecMetadataText {
  param(
    [Parameter(Mandatory)]
    [xml] $Document,

    [Parameter(Mandatory)]
    [string] $Name,

    [Parameter(Mandatory)]
    [string] $Value
  )

  $NamespaceManager = New-NuspecNamespaceManager $Document
  $Metadata = $Document.SelectSingleNode('/n:package/n:metadata', $NamespaceManager)
  $Element = $Metadata.SelectSingleNode("n:$Name", $NamespaceManager)
  if (-not $Element) {
    $Element = $Document.CreateElement($Name, $Document.DocumentElement.NamespaceURI)
    [void] $Metadata.AppendChild($Element)
  }

  $Element.InnerText = $Value
}

function ConvertTo-VendorPackageId {
  param(
    [Parameter(Mandatory)]
    [string] $Id
  )

  if (-not $Id.StartsWith($OriginalIdPrefix, [System.StringComparison]::Ordinal)) {
    throw "Package ID '$Id' does not start with '$OriginalIdPrefix'."
  }

  return $VendorIdPrefix + $Id.Substring($OriginalIdPrefix.Length)
}

function Convert-PackageIdReferences {
  param(
    [Parameter(Mandatory)]
    [AllowEmptyString()]
    [string] $Text,

    [Parameter(Mandatory)]
    [hashtable] $IdMap
  )

  # Rewrites exact package IDs and package ID families followed by an MSBuild
  # property, such as Turso.Data.NativeAot.$(RuntimeIdentifier). Assembly file
  # names (Turso.Data.dll) and namespaces that are not package IDs are kept.
  $Pattern = '(?<![\w.])' + [regex]::Escape($OriginalIdPrefix.TrimEnd('.')) + '(?:\.[A-Za-z0-9][\w-]*)+'
  $KnownIds = @($IdMap.Keys)
  return [regex]::Replace($Text, $Pattern, {
      param($Match)

      $Token = $Match.Value
      if ($IdMap.ContainsKey($Token)) {
        return $IdMap[$Token]
      }

      $Next = $Text.Substring($Match.Index + $Match.Length)
      if ($Next.StartsWith('.$(') -and @($KnownIds | Where-Object { $_.StartsWith("$Token.", [System.StringComparison]::Ordinal) }).Count -gt 0) {
        return ConvertTo-VendorPackageId $Token
      }

      return $Token
    })
}

function Convert-ReadmePackageReferences {
  param(
    [Parameter(Mandatory)]
    [AllowEmptyString()]
    [string] $Text,

    [Parameter(Mandatory)]
    [hashtable] $IdMap
  )

  # README code samples also use Turso namespaces that match package IDs, such
  # as Turso.EntityFrameworkCore.Sqlite, so only package install contexts are
  # rewritten here.
  foreach ($OldId in @($IdMap.Keys | Sort-Object Length -Descending)) {
    $NewId = $IdMap[$OldId]
    $EscapedId = [regex]::Escape($OldId)
    $Text = [regex]::Replace($Text, "(dotnet\s+add\s+(?:\S+\s+)?package\s+)$EscapedId(?![\w.-])", "`${1}$NewId")
    $Text = [regex]::Replace($Text, "(Install-Package\s+)$EscapedId(?![\w.-])", "`${1}$NewId")
    $Text = [regex]::Replace($Text, "(<PackageReference\s+Include=`")$EscapedId(`")", "`${1}$NewId`${2}")
    $Text = [regex]::Replace($Text, "(<PackageVersion\s+Include=`")$EscapedId(`")", "`${1}$NewId`${2}")
  }

  return $Text
}

function Convert-DependencyVersion {
  param(
    [Parameter(Mandatory)]
    [string] $Version
  )

  if ($Version.StartsWith('[') -and $Version.EndsWith(']') -and $Version -notmatch ',') {
    return "[$PackageVersion]"
  }
  if ($Version -match '^[\[\(]') {
    throw "Unsupported dependency version range '$Version' on a vendored package dependency."
  }

  return $PackageVersion
}

function Add-ZipEntry {
  param(
    [Parameter(Mandatory)]
    [System.IO.Compression.ZipArchive] $Archive,

    [Parameter(Mandatory)]
    [string] $Name,

    [Parameter(Mandatory)]
    [AllowEmptyCollection()]
    [byte[]] $Bytes,

    [Parameter(Mandatory)]
    [System.DateTimeOffset] $LastWriteTime
  )

  $Entry = $Archive.CreateEntry($Name, [System.IO.Compression.CompressionLevel]::Optimal)
  $Entry.LastWriteTime = $LastWriteTime
  $Stream = $Entry.Open()
  try {
    $Stream.Write($Bytes, 0, $Bytes.Length)
  } finally {
    $Stream.Dispose()
  }
}

function ConvertTo-Utf8Bytes {
  param(
    [Parameter(Mandatory)]
    [AllowEmptyString()]
    [string] $Text
  )

  return ,([System.Text.UTF8Encoding]::new($false)).GetBytes($Text)
}

function ConvertTo-XmlBytes {
  param(
    [Parameter(Mandatory)]
    [xml] $Document
  )

  $Settings = [System.Xml.XmlWriterSettings]::new()
  $Settings.Encoding = [System.Text.UTF8Encoding]::new($false)
  $Settings.Indent = $true
  $Buffer = [System.IO.MemoryStream]::new()
  $Writer = [System.Xml.XmlWriter]::Create($Buffer, $Settings)
  try {
    $Document.Save($Writer)
  } finally {
    $Writer.Dispose()
  }

  return ,$Buffer.ToArray()
}

$NormalizedPackageVersion = Get-NormalizedNuGetVersion $PackageVersion

if (-not (Test-Path -LiteralPath $InputDirectory -PathType Container)) {
  throw "Input directory not found: $InputDirectory"
}

$InputPackages = @(Get-ChildItem -LiteralPath $InputDirectory -Filter '*.nupkg' -File | Where-Object { $_.Name -notlike '*.symbols.nupkg' } | Sort-Object Name)
if ($InputPackages.Count -eq 0) {
  throw "No .nupkg files found in $InputDirectory"
}

New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
$OutputDirectory = (Resolve-Path -LiteralPath $OutputDirectory).Path

# First pass: discover every package ID so dependencies between vendored
# packages can be rewritten consistently.
$IdMap = @{}
$PackageIds = @{}
foreach ($InputPackage in $InputPackages) {
  $Archive = [System.IO.Compression.ZipFile]::OpenRead($InputPackage.FullName)
  try {
    [xml] $Nuspec = Read-ZipEntryText (Get-NuspecEntry -Archive $Archive -PackagePath $InputPackage.FullName)
    $Id = Get-NuspecMetadataValue -Document $Nuspec -Name 'id'
    if ($IdMap.ContainsKey($Id)) {
      throw "Package ID '$Id' appears more than once in $InputDirectory."
    }

    $IdMap[$Id] = ConvertTo-VendorPackageId $Id
    $PackageIds[$InputPackage.FullName] = $Id
  } finally {
    $Archive.Dispose()
  }
}

$Results = @()
foreach ($InputPackage in $InputPackages) {
  $OldId = $PackageIds[$InputPackage.FullName]
  $NewId = $IdMap[$OldId]
  $OutputPath = Join-Path $OutputDirectory "$NewId.$NormalizedPackageVersion.nupkg"
  Remove-Item -LiteralPath $OutputPath -Force -ErrorAction SilentlyContinue

  $SourceArchive = [System.IO.Compression.ZipFile]::OpenRead($InputPackage.FullName)
  $TargetStream = [System.IO.File]::Open($OutputPath, [System.IO.FileMode]::CreateNew)
  $TargetArchive = [System.IO.Compression.ZipArchive]::new($TargetStream, [System.IO.Compression.ZipArchiveMode]::Create)
  try {
    $NuspecEntry = Get-NuspecEntry -Archive $SourceArchive -PackagePath $InputPackage.FullName
    [xml] $Nuspec = Read-ZipEntryText $NuspecEntry
    $OriginalVersion = Get-NuspecMetadataValue -Document $Nuspec -Name 'version'
    $ReadmeFile = Get-NuspecMetadataValue -Document $Nuspec -Name 'readme'
    $Authors = Get-NuspecMetadataValue -Document $Nuspec -Name 'authors'
    $Description = Get-NuspecMetadataValue -Document $Nuspec -Name 'description'
    $SourceVersion = if ($UpstreamVersion) { $UpstreamVersion } else { $OriginalVersion }

    Set-NuspecMetadataText -Document $Nuspec -Name 'id' -Value $NewId
    Set-NuspecMetadataText -Document $Nuspec -Name 'version' -Value $PackageVersion
    if ($Authors -and $Authors -notmatch [regex]::Escape($VendorName)) {
      Set-NuspecMetadataText -Document $Nuspec -Name 'authors' -Value "$Authors, $VendorName"
    }
    Set-NuspecMetadataText -Document $Nuspec -Name 'owners' -Value $VendorName
    Set-NuspecMetadataText -Document $Nuspec -Name 'projectUrl' -Value $RepositoryUrl
    $VendorNote = "$VendorName distribution of $OldId $SourceVersion built from $RepositoryUrl."
    Set-NuspecMetadataText -Document $Nuspec -Name 'description' -Value $(if ($Description) { "$Description`n`n$VendorNote" } else { $VendorNote })

    $NamespaceManager = New-NuspecNamespaceManager $Nuspec
    $Repository = $Nuspec.SelectSingleNode('/n:package/n:metadata/n:repository', $NamespaceManager)
    if (-not $Repository) {
      $Repository = $Nuspec.CreateElement('repository', $Nuspec.DocumentElement.NamespaceURI)
      [void] $Nuspec.SelectSingleNode('/n:package/n:metadata', $NamespaceManager).AppendChild($Repository)
    }
    $Repository.SetAttribute('type', 'git')
    $Repository.SetAttribute('url', $RepositoryUrl)
    if ($RepositoryCommit) {
      $Repository.SetAttribute('commit', $RepositoryCommit)
    } else {
      $Repository.RemoveAttribute('commit')
    }
    $Repository.RemoveAttribute('branch')

    $RewrittenDependencies = 0
    foreach ($Dependency in @($Nuspec.SelectNodes('//n:dependency', $NamespaceManager))) {
      $DependencyId = $Dependency.GetAttribute('id')
      if ($IdMap.ContainsKey($DependencyId)) {
        $Dependency.SetAttribute('id', $IdMap[$DependencyId])
        $Dependency.SetAttribute('version', (Convert-DependencyVersion $Dependency.GetAttribute('version')))
        $RewrittenDependencies++
      } elseif ($DependencyId.StartsWith($OriginalIdPrefix, [System.StringComparison]::Ordinal)) {
        throw "$OldId depends on $DependencyId, which is not part of the vendored package set in $InputDirectory."
      }
    }

    foreach ($Entry in @($SourceArchive.Entries | Sort-Object FullName)) {
      $Name = $Entry.FullName
      $Extension = [System.IO.Path]::GetExtension($Name).ToLowerInvariant()

      if ($Name -eq '.signature.p7s') {
        Write-Warning "Dropping package signature from $($InputPackage.Name); the package contents change during vendoring."
        continue
      }

      if ($Entry -eq $NuspecEntry) {
        Add-ZipEntry -Archive $TargetArchive -Name "$NewId.nuspec" -Bytes (ConvertTo-XmlBytes $Nuspec) -LastWriteTime $Entry.LastWriteTime
        continue
      }

      if ($Name -eq '_rels/.rels') {
        $Text = (Read-ZipEntryText $Entry).Replace("/$OldId.nuspec", "/$NewId.nuspec")
        Add-ZipEntry -Archive $TargetArchive -Name $Name -Bytes (ConvertTo-Utf8Bytes $Text) -LastWriteTime $Entry.LastWriteTime
        continue
      }

      if ($Name -like 'package/services/metadata/core-properties/*.psmdcp') {
        [xml] $CoreProperties = Read-ZipEntryText $Entry
        foreach ($Node in @($CoreProperties.DocumentElement.ChildNodes)) {
          switch ($Node.LocalName) {
            'identifier' { $Node.InnerText = $NewId }
            'version' { $Node.InnerText = $PackageVersion }
            'description' { $Node.InnerText = (Get-NuspecMetadataValue -Document $Nuspec -Name 'description') }
            'creator' { $Node.InnerText = (Get-NuspecMetadataValue -Document $Nuspec -Name 'authors') }
          }
        }
        Add-ZipEntry -Archive $TargetArchive -Name $Name -Bytes (ConvertTo-XmlBytes $CoreProperties) -LastWriteTime $Entry.LastWriteTime
        continue
      }

      if ($Name -match '^(build|buildTransitive|buildMultiTargeting)/' -and ($Extension -eq '.props' -or $Extension -eq '.targets')) {
        $TargetName = $Name
        if ($Name -match "^(build|buildTransitive|buildMultiTargeting)/(?:[^/]+/)?$([regex]::Escape($OldId))\.(props|targets)$") {
          $TargetName = $Name.Substring(0, $Name.Length - $Entry.Name.Length) + "$NewId.$($Matches[2])"
        }
        $Text = Convert-PackageIdReferences -Text (Read-ZipEntryText $Entry) -IdMap $IdMap
        Add-ZipEntry -Archive $TargetArchive -Name $TargetName -Bytes (ConvertTo-Utf8Bytes $Text) -LastWriteTime $Entry.LastWriteTime
        continue
      }

      if ($ReadmeFile -and $Name -eq $ReadmeFile.Replace('\', '/')) {
        $Text = Convert-ReadmePackageReferences -Text (Read-ZipEntryText $Entry) -IdMap $IdMap
        Add-ZipEntry -Archive $TargetArchive -Name $Name -Bytes (ConvertTo-Utf8Bytes $Text) -LastWriteTime $Entry.LastWriteTime
        continue
      }

      Add-ZipEntry -Archive $TargetArchive -Name $Name -Bytes (Read-ZipEntryBytes $Entry) -LastWriteTime $Entry.LastWriteTime
    }
  } finally {
    $TargetArchive.Dispose()
    $TargetStream.Dispose()
    $SourceArchive.Dispose()
  }

  # Validate the rewritten package identity and imports before moving on.
  $Verify = [System.IO.Compression.ZipFile]::OpenRead($OutputPath)
  try {
    [xml] $VerifyNuspec = Read-ZipEntryText (Get-NuspecEntry -Archive $Verify -PackagePath $OutputPath)
    if ((Get-NuspecMetadataValue -Document $VerifyNuspec -Name 'id') -ne $NewId) {
      throw "Vendored package $OutputPath does not have ID $NewId."
    }
    $StaleImports = @($Verify.Entries | Where-Object { $_.FullName -match "^(build|buildTransitive|buildMultiTargeting)/(?:[^/]+/)?$([regex]::Escape($OldId))\.(props|targets)$" })
    if ($StaleImports.Count -gt 0) {
      throw "Vendored package $OutputPath still contains MSBuild imports named after $OldId."
    }
  } finally {
    $Verify.Dispose()
  }

  Write-Host "Vendored $OldId $OriginalVersion -> $NewId $PackageVersion ($RewrittenDependencies dependency reference(s) rewritten)"
  $Results += [pscustomobject]@{
    OriginalId = $OldId
    OriginalVersion = $OriginalVersion
    PackageId = $NewId
    PackageVersion = $PackageVersion
    Path = $OutputPath
  }
}

Write-Output "PackageVersion=$PackageVersion"
Write-Output "NormalizedPackageVersion=$NormalizedPackageVersion"
Write-Output "PackageCount=$($Results.Count)"
foreach ($Result in $Results) {
  Write-Output "Package=$($Result.PackageId)"
}

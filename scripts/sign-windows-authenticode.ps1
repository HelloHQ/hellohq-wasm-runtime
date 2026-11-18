# Authenticode-sign the Windows app with Azure Artifact Signing, or check that
# it is signed.
#
#   pwsh scripts/sign-windows-authenticode.ps1 -Path <runner\Release> [-Path <setup.exe>]
#   pwsh scripts/sign-windows-authenticode.ps1 -Preflight
#   pwsh scripts/sign-windows-authenticode.ps1 -VerifyOnly -Path <runner\Release> -Path <setup.exe>
#
# Signs every .exe and .dll under each -Path that has no signature yet, with a
# SHA-256 digest and an RFC 3161 timestamp. Microsoft's certificates live for
# about three days; without the timestamp a signature stops validating when the
# day's certificate expires. Files that already carry a valid signature (e.g.
# Microsoft's WebView2Loader.dll) are left as they are. A file whose signature
# is present but invalid is an error: something changed it after signing.
#
# Not signed, on purpose: Flutter's asset folder (data\). The Tier-1 sidecar
# launcher ships there with a pinned SHA-256 that the app checks before running
# it (PluginSidecarLauncher); signing changes the bytes, so the app would refuse
# it and fall back to the weaker in-process sandbox. The app extracts and runs
# it itself, so SmartScreen never sees it.
#
# The signing client is Microsoft's dlib for SignTool, pinned below by version
# and SHA-256 and downloaded from nuget.org. It needs, on this machine:
#   - the Windows SDK's signtool.exe (10.0.22621 or newer; Visual Studio's C++
#     workload installs it),
#   - the .NET 8 (or newer) x64 runtime: winget install Microsoft.DotNet.Runtime.8
#
# Credentials come from the environment only, and only the app registration's
# client secret is used (EnvironmentCredential; every other credential source is
# excluded, so signing never falls back to an interactive sign-in):
#   AZURE_TENANT_ID, AZURE_CLIENT_ID, AZURE_CLIENT_SECRET,
#   TRUSTED_SIGNING_ENDPOINT, TRUSTED_SIGNING_ACCOUNT, TRUSTED_SIGNING_CERT_PROFILE
# The endpoint is the account's regional one, e.g. https://jpe.codesigning.azure.net.
#
# -Preflight checks the tools and fetches the client without signing anything,
# so a missing tool fails before the build rather than after it.
param(
  [string[]]$Path = @(),
  [switch]$Preflight,
  [switch]$VerifyOnly
)
$ErrorActionPreference = 'Stop'

# Microsoft.ArtifactSigning.Client on nuget.org. To bump: pick a version at
# least two weeks old, download its .nupkg and replace both values.
$ClientVersion = '1.0.146'
$ClientSha256 = '7761e5d8f31e54b764da21d33cd8b0a2940e1b05526c7dfcb5e94b0431426f70'
$TimestampUrl = 'http://timestamp.acs.microsoft.com'
$SigningVariables = @('AZURE_TENANT_ID', 'AZURE_CLIENT_ID', 'AZURE_CLIENT_SECRET',
  'TRUSTED_SIGNING_ENDPOINT', 'TRUSTED_SIGNING_ACCOUNT', 'TRUSTED_SIGNING_CERT_PROFILE')

if (-not $IsWindows) { throw 'Authenticode signing runs on Windows only.' }
if (-not $Preflight -and $Path.Count -eq 0) { throw 'give at least one -Path (a directory or a file)' }

# Every .exe and .dll to sign or check, outside Flutter's asset folder.
function Get-Binaries([string[]]$Paths) {
  $files = foreach ($item in $Paths) {
    $resolved = (Resolve-Path -LiteralPath $item).Path
    if (Test-Path -LiteralPath $resolved -PathType Leaf) {
      Get-Item -LiteralPath $resolved
    } else {
      $assets = Join-Path $resolved 'data'
      Get-ChildItem -LiteralPath $resolved -Recurse -File |
        Where-Object { ($_.Extension -in @('.exe', '.dll')) -and -not $_.FullName.StartsWith("$assets\", [StringComparison]::OrdinalIgnoreCase) }
    }
  }
  @($files | Sort-Object FullName -Unique)
}

function Get-SignerName($Signature) {
  if (-not $Signature.SignerCertificate) { return '' }
  $Signature.SignerCertificate.GetNameInfo([System.Security.Cryptography.X509Certificates.X509NameType]::SimpleName, $false)
}

if ($VerifyOnly) {
  $bad = [System.Collections.Generic.List[string]]::new()
  foreach ($file in Get-Binaries $Path) {
    $sig = Get-AuthenticodeSignature -LiteralPath $file.FullName
    $why = if ($sig.Status -ne 'Valid') { "$($sig.Status)" }
           elseif (-not $sig.TimeStamperCertificate) { 'no timestamp' }
           else { '' }
    $line = '{0,-8} {1} ({2})' -f $(if ($why) { 'FAIL' } else { 'ok' }), $file.Name, $(if ($why) { $why } else { Get-SignerName $sig })
    Write-Host $line
    if ($why) { $bad.Add("$($file.FullName): $why") }
  }
  if ($bad.Count -gt 0) {
    $bad | ForEach-Object { Write-Host "  not validly signed: $_" -ForegroundColor Red }
    throw "$($bad.Count) binaries are not validly Authenticode-signed and timestamped"
  }
  Write-Host 'every binary is Authenticode-signed and timestamped'
  return
}

# --- Tools -------------------------------------------------------------------
$missing = @($SigningVariables | Where-Object { -not [Environment]::GetEnvironmentVariable($_) })
if ($missing.Count -gt 0) { throw "Authenticode signing needs these environment variables: $($missing -join ', ')" }
$Endpoint = $env:TRUSTED_SIGNING_ENDPOINT.TrimEnd('/')
if ($Endpoint -notmatch '^https://[a-z0-9]+\.codesigning\.azure\.net$') {
  throw "TRUSTED_SIGNING_ENDPOINT must be a regional https://<region>.codesigning.azure.net endpoint"
}

function Find-SignTool {
  foreach ($root in @("${env:ProgramFiles(x86)}\Windows Kits\10\bin", "$env:ProgramFiles\Windows Kits\10\bin")) {
    if (-not (Test-Path -LiteralPath $root)) { continue }
    $exe = Get-ChildItem -LiteralPath $root -Directory |
      Where-Object { $_.Name -match '^10\.0\.\d+\.\d+$' -and ([version]$_.Name) -ge [version]'10.0.22621.0' } |
      Sort-Object { [version]$_.Name } -Descending |
      ForEach-Object { Join-Path $_.FullName 'x64\signtool.exe' } |
      Where-Object { Test-Path -LiteralPath $_ } |
      Select-Object -First 1
    if ($exe) { return $exe }
  }
  return $null
}
$SignTool = Find-SignTool
if (-not $SignTool) { throw 'signtool.exe from Windows SDK 10.0.22621 or newer not found (install the Windows SDK, or Visual Studio with Desktop C++)' }

$runtimes = @()
$dotnet = Get-Command dotnet -ErrorAction SilentlyContinue
if ($dotnet) { $runtimes = @(& $dotnet.Source --list-runtimes 2>$null) }
$hasNet8 = $runtimes | Where-Object { $_ -match '^Microsoft\.NETCore\.App (\d+)\.' -and [int]$Matches[1] -ge 8 }
if (-not $hasNet8) { throw 'the signing client needs the .NET 8 (or newer) x64 runtime: winget install Microsoft.DotNet.Runtime.8' }

# The client package is cached per user and re-checked against the pin on every
# run; it is unpacked fresh into a temporary directory each time.
$cacheDir = Join-Path $env:LOCALAPPDATA 'HelloHQ\artifact-signing'
New-Item -ItemType Directory -Force -Path $cacheDir | Out-Null
$package = Join-Path $cacheDir "microsoft.artifactsigning.client.$ClientVersion.nupkg"
function Test-Pin([string]$File) { (Get-FileHash -Algorithm SHA256 -LiteralPath $File).Hash.ToLowerInvariant() -eq $ClientSha256 }
if (-not (Test-Path -LiteralPath $package) -or -not (Test-Pin $package)) {
  $part = "$package.part"
  Invoke-WebRequest -UseBasicParsing -OutFile $part `
    -Uri "https://api.nuget.org/v3-flatcontainer/microsoft.artifactsigning.client/$ClientVersion/microsoft.artifactsigning.client.$ClientVersion.nupkg"
  if (-not (Test-Pin $part)) {
    Remove-Item -LiteralPath $part -Force
    throw "the Artifact Signing client $ClientVersion from nuget.org does not match its pinned SHA-256"
  }
  Move-Item -LiteralPath $part -Destination $package -Force
}
Write-Host "signtool: $SignTool"
Write-Host "Artifact Signing client $ClientVersion (sha256 ok), endpoint $Endpoint"
if ($Preflight) { return }

# --- Sign --------------------------------------------------------------------
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("hellohq-sign-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $work | Out-Null
try {
  $zip = Join-Path $work 'client.zip'
  Copy-Item -LiteralPath $package -Destination $zip
  Expand-Archive -LiteralPath $zip -DestinationPath (Join-Path $work 'client')
  $dlib = Join-Path $work 'client\bin\x64\Azure.CodeSigning.Dlib.dll'
  if (-not (Test-Path -LiteralPath $dlib)) { throw 'the signing client package has no bin\x64\Azure.CodeSigning.Dlib.dll' }

  $metadata = Join-Path $work 'metadata.json'
  [ordered]@{
    Endpoint               = $Endpoint
    CodeSigningAccountName = $env:TRUSTED_SIGNING_ACCOUNT
    CertificateProfileName = $env:TRUSTED_SIGNING_CERT_PROFILE
    ExcludeCredentials     = @('ManagedIdentityCredential', 'WorkloadIdentityCredential',
      'SharedTokenCacheCredential', 'VisualStudioCredential', 'VisualStudioCodeCredential',
      'AzureCliCredential', 'AzurePowerShellCredential', 'AzureDeveloperCliCredential',
      'InteractiveBrowserCredential')
  } | ConvertTo-Json | Set-Content -LiteralPath $metadata -Encoding utf8NoBOM

  $toSign = [System.Collections.Generic.List[string]]::new()
  foreach ($file in Get-Binaries $Path) {
    $sig = Get-AuthenticodeSignature -LiteralPath $file.FullName
    switch ($sig.Status) {
      'NotSigned' { $toSign.Add($file.FullName) }
      'Valid' { Write-Host "already signed: $($file.Name) ($(Get-SignerName $sig))" }
      default { throw "$($file.FullName) has an invalid signature ($($sig.Status)); it changed after it was signed" }
    }
  }
  Write-Host "signing $($toSign.Count) files"

  # A batch per call keeps the command line short; each call authenticates once.
  for ($i = 0; $i -lt $toSign.Count; $i += 25) {
    $batch = @($toSign[$i..([Math]::Min($i + 25, $toSign.Count) - 1)])
    & $SignTool sign /fd SHA256 /tr $TimestampUrl /td SHA256 /dlib $dlib /dmdf $metadata @batch
    if ($LASTEXITCODE -ne 0) { throw "signtool failed (exit $LASTEXITCODE)" }
  }

  foreach ($f in $toSign) {
    $sig = Get-AuthenticodeSignature -LiteralPath $f
    if ($sig.Status -ne 'Valid' -or -not $sig.TimeStamperCertificate) {
      throw "$f is not validly signed and timestamped after signing ($($sig.Status))"
    }
  }
  if ($toSign.Count -gt 0) {
    Write-Host "signed $($toSign.Count) files as $(Get-SignerName (Get-AuthenticodeSignature -LiteralPath $toSign[0]))"
  }
}
finally {
  Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}

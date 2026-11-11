# User-only installation. No elevation, provider sign-ins, task registration or host restart.
#   irm https://getcupola.app/install.ps1 | iex
# A piped script cannot take parameters, so each has an environment variable
# (CUPOLA_* wins over the older WONDER_*): UPDATE_URL, CHANNEL, VERSION, INSTALL_DIR, NO_PATH.
param(
  [string]$Feed,
  [string]$Channel,
  [string]$Version,
  [string]$Prefix,
  [switch]$NoPath
)
$savedPreference = $ErrorActionPreference
# Run in a child scope so `irm | iex` leaves no variables or functions in the caller's session.
$ErrorActionPreference = 'Stop'
try { & {
# The newest GitHub release holds stable.json and the archives side by side.
$DefaultFeed = 'https://github.com/swaymun/cupola/releases/latest/download'
function Setting([string]$Name) {
  foreach ($prefixName in 'CUPOLA_', 'WONDER_') {
    $value = [Environment]::GetEnvironmentVariable($prefixName + $Name)
    if ($value) { return $value }
  }
}
if (-not $Feed) { $Feed = Setting 'UPDATE_URL' }
if (-not $Feed) { $Feed = $DefaultFeed }
if (-not $Channel) { $Channel = if (Setting 'CHANNEL') { Setting 'CHANNEL' } else { 'stable' } }
if ($Channel -cnotin 'stable', 'nightly') { throw 'CUPOLA_CHANNEL must be stable or nightly.' }
if (-not $Version) { $Version = Setting 'VERSION' }
if (-not $Prefix) { $Prefix = Setting 'INSTALL_DIR' }
if (-not $Prefix) { $Prefix = Join-Path $env:LOCALAPPDATA 'Programs\Cupola' }
if (-not $NoPath -and (Setting 'NO_PATH') -in '1', 'true') { $NoPath = [switch]$true }
Add-Type -AssemblyName System.Net.Http
Add-Type -AssemblyName System.IO.Compression.FileSystem

function Assert-Url([uri]$Url) {
  if ($Url.UserInfo -or ($Url.Scheme -ne 'https' -and -not ($Url.Scheme -eq 'http' -and $Url.Host -in @('localhost', '127.0.0.1', '[::1]')))) {
    throw 'Release URLs must use HTTPS (HTTP is allowed only on loopback for local acceptance).'
  }
}
function Get-ReleaseFile([uri]$Url, [string]$Path) {
  $handler = New-Object System.Net.Http.HttpClientHandler
  $handler.AllowAutoRedirect = $false
  $client = New-Object System.Net.Http.HttpClient($handler)
  $client.Timeout = [TimeSpan]::FromMinutes(10)
  try {
    for ($redirects = 0; $redirects -le 5; $redirects++) {
      Assert-Url $Url
      $response = $client.GetAsync($Url, [System.Net.Http.HttpCompletionOption]::ResponseHeadersRead).GetAwaiter().GetResult()
      try {
        if ([int]$response.StatusCode -in @(301, 302, 303, 307, 308)) {
          $Url = New-Object uri($Url, $response.Headers.Location)
          continue
        }
        $response.EnsureSuccessStatusCode() | Out-Null
        $output = [IO.File]::Create($Path)
        try { $response.Content.CopyToAsync($output).GetAwaiter().GetResult() } finally { $output.Dispose() }
        return
      } finally { $response.Dispose() }
    }
    throw 'Too many release redirects.'
  } finally { $client.Dispose(); $handler.Dispose() }
}
function Assert-Version([string]$Value) {
  if ($Value -cnotmatch '^[0-9][A-Za-z0-9.+-]{0,63}$') { throw 'Invalid release version.' }
}
if ($Version -and $Feed.TrimEnd('/') -eq $DefaultFeed) { throw "A version needs a feed with versions/$Version.json, which GitHub releases cannot hold. Pin a release with -Feed https://github.com/swaymun/cupola/releases/download/vX.Y.Z (that release's folder)." }
if ($Version) { Assert-Version $Version }
$Prefix = [IO.Path]::GetFullPath($Prefix)
if ($Prefix -eq $env:USERPROFILE -or $Prefix.Length -lt 8) { throw 'Choose a dedicated installation directory.' }
if (Test-Path $Prefix) {
  if ((Get-Item $Prefix).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Installation directory cannot be a link.' }
  if (Get-ChildItem -Force $Prefix) { throw 'Installation directory already exists. Use cupola update, or uninstall before reinstalling.' }
}
$arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'aarch64' } else { 'x86_64' }
$manifestName = if ($Version) { "versions/$Version.json" } else { "$Channel.json" }
$manifestUrl = [uri]($Feed.TrimEnd('/') + '/' + $manifestName)
$parent = Split-Path $Prefix
New-Item -ItemType Directory -Force $parent | Out-Null
$stage = Join-Path $parent ('.cupola-install-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory $stage | Out-Null
try {
  $manifestPath = Join-Path $stage 'release.json'
  Get-ReleaseFile $manifestUrl $manifestPath
  $manifest = Get-Content -Raw $manifestPath | ConvertFrom-Json
  Assert-Version $manifest.version
  if ($Version -and $manifest.version -ne $Version) { throw 'The manifest does not match the requested version.' }
  $artifact = $manifest.artifacts."windows-$arch"
  if (-not $artifact -or $artifact.sha256 -cnotmatch '^[0-9a-f]{64}$') { throw "No SHA256-verified artifact for windows-$arch." }
  $archive = Join-Path $stage 'release.zip'
  Get-ReleaseFile (New-Object uri($manifestUrl, $artifact.url)) $archive
  if ((Get-FileHash -Algorithm SHA256 $archive).Hash.ToLowerInvariant() -cne $artifact.sha256) { throw 'SHA256 mismatch; nothing installed.' }
  $zip = [IO.Compression.ZipFile]::OpenRead($archive)
  try {
    foreach ($entry in $zip.Entries) {
      if ($entry.FullName -match '(^[/\\]|(^|[/\\])\.\.([/\\]|$)|:)' -or (($entry.ExternalAttributes -shr 16) -band 0xF000) -eq 0xA000) { throw 'Unsafe ZIP entry.' }
    }
  } finally { $zip.Dispose() }
  $unpacked = Join-Path $stage 'unpacked'
  [IO.Compression.ZipFile]::ExtractToDirectory($archive, $unpacked)
  foreach ($name in @('cupola.exe', 'wonderd.exe', 'uninstall.ps1')) {
    if (-not (Test-Path (Join-Path $unpacked $name) -PathType Leaf)) { throw "Release lacks $name." }
  }
  $reported = & (Join-Path $unpacked 'cupola.exe') version --json
  if ($LASTEXITCODE -ne 0 -or ($reported | ConvertFrom-Json).version -ne $manifest.version) { throw 'The release executable reports a different version.' }
  $release = Join-Path $Prefix ('versions\' + $manifest.version)
  New-Item -ItemType Directory -Force (Split-Path $release) | Out-Null
  Move-Item $unpacked $release
  $binDir = Join-Path $Prefix 'bin'
  New-Item -ItemType Directory $binDir | Out-Null
  # Immutable CMD launchers avoid symlinks and never change under an updater.
  [IO.File]::WriteAllText((Join-Path $Prefix 'current-version'), "$($manifest.version)`r`n", [Text.Encoding]::ASCII)
  foreach ($name in @('cupola', 'wonderd')) {
    $launcher = @"
@echo off
setlocal
set "CUPOLA_CLI_RELEASE="
set /p "CUPOLA_CLI_RELEASE="<"%~dp0..\current-version"
if not defined CUPOLA_CLI_RELEASE exit /b 1
"%~dp0..\versions\%CUPOLA_CLI_RELEASE%\$name.exe" %*
exit /b %errorlevel%
"@
    [IO.File]::WriteAllText((Join-Path $binDir "$name.cmd"), ($launcher -replace "`r?`n", "`r`n") + "`r`n", [Text.Encoding]::ASCII)
  }
  Copy-Item (Join-Path $release 'uninstall.ps1') $Prefix
  $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  $addedPath = -not $NoPath -and ($userPath -split ';') -notcontains $binDir
  if ($addedPath) { [Environment]::SetEnvironmentVariable('Path', "$binDir;$userPath", 'User') }
  @{owner='wonder-cli-v1'; prefix=$Prefix; binDir=$binDir; platform='windows'; channel=$Channel; feed=$Feed.TrimEnd('/')+'/'; addedPath=[bool]$addedPath} |
    ConvertTo-Json | Set-Content -Encoding UTF8 (Join-Path $Prefix 'install.json')
  Write-Output "Installed Cupola $($manifest.version) ($Channel) in $Prefix."
  Write-Output "Open a new terminal, then: cupola init; cupola start; cupola pair"
  Write-Output "Uninstall (keeps user data): powershell -ExecutionPolicy Bypass -File `"$Prefix\uninstall.ps1`""
} finally { Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue }
} } finally { $ErrorActionPreference = $savedPreference }

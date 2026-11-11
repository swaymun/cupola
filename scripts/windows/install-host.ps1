# Installs the experimental Windows host for this user from built binaries and
# registers it to start at sign-in. Rerunnable: stops a running host first.
# Signs in to nothing: Tailscale, Codex, Claude Code and OpenCode sign-ins and
# pairing the phone are the owner's steps.
#
#   scripts\windows\install-host.ps1 -Bin <dir with wonderd.exe and cupola.exe>
#       [-Prefix %LOCALAPPDATA%\Programs\Cupola] [-Python <python.exe>] [-NoService] [-NoStart]
#
# Layout: <Prefix>\bin\{wonderd,cupola}.exe and <Prefix>\share\wonder\
# {claude-runtime,acp-runtime,computer-windows}, which is where `cupola`
# looks for them. -Python creates <Prefix>\python (a venv with the screen
# helper's requirements) and is used for `init --computer`.
#
# Every step that can fail stops the install before anything is replaced or
# reported as installed: runtimes are prepared beside the old ones and swapped
# in only after their dependencies installed.
param(
  [Parameter(Mandatory = $true)][string]$Bin,
  [string]$Prefix = (Join-Path $env:LOCALAPPDATA 'Programs\Cupola'),
  [string]$Python,
  [switch]$NoService,
  [switch]$NoStart
)
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path

# Native commands do not throw on a nonzero exit; check each one.
# Windows PowerShell 5.1 turns a native command's stderr lines (npm's "notice" about a new npm) into
# terminating errors under -ErrorAction Stop when they are redirected with 2>&1; the exit code decides.
function Invoke-Native([string]$What, [scriptblock]$Command) {
  $saved = $ErrorActionPreference
  $ErrorActionPreference = 'Continue'
  try { & $Command } finally { $ErrorActionPreference = $saved }
  if ($LASTEXITCODE -ne 0) { throw "$What failed (exit $LASTEXITCODE); nothing was reported as installed" }
}

foreach ($exe in 'wonderd.exe', 'cupola.exe') {
  if (-not (Test-Path (Join-Path $Bin $exe))) { throw "$exe is not in $Bin; build with scripts/windows/cargo-windows build -p wonderd --features experimental-headless --bins" }
}
$binDir = Join-Path $Prefix 'bin'
$share = Join-Path $Prefix 'share\wonder'
New-Item -ItemType Directory -Force $binDir, $share | Out-Null

# Prepare the Node runtimes first: a failed download leaves the installed
# runtimes and the running host untouched.
$staged = @{}
try {
  foreach ($runtime in 'claude-runtime', 'acp-runtime') {
    $stage = Join-Path $share ".$runtime.new"
    if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
    New-Item -ItemType Directory -Force $stage | Out-Null
    $staged[$runtime] = $stage
    Get-ChildItem (Join-Path $repo "services\$runtime") -Force |
      Where-Object { $_.Name -notin 'node_modules', 'test' } |
      Copy-Item -Destination $stage -Recurse -Force
    if (Test-Path (Join-Path $stage 'package-lock.json')) {
      Push-Location $stage
      try {
        Invoke-Native "npm ci for $runtime" { npm ci --omit=dev --no-audit --no-fund 2>&1 | Select-Object -Last 1 }
      } finally { Pop-Location }
    }
  }

  $venvPython = $null
  if ($Python) {
    $venv = Join-Path $Prefix 'python'
    $venvPython = Join-Path $venv 'Scripts\python.exe'
    if (-not (Test-Path $venvPython)) { Invoke-Native 'creating the Python environment' { & $Python -m venv $venv } }
    Invoke-Native 'installing the screen helper requirements' {
      & $venvPython -m pip install --quiet --disable-pip-version-check -r (Join-Path $repo 'native\computer-use-windows\requirements.txt')
    }
  }

  # Everything is ready: stop the running host and swap the new files in.
  foreach ($old in 'cupola.exe', 'wonder.exe', 'wonder-host.exe') {
    $installed = Join-Path $binDir $old
    if (Test-Path $installed) { & $installed stop 2>$null | Out-Null }
  }
  Copy-Item -Force (Join-Path $Bin 'wonderd.exe'), (Join-Path $Bin 'cupola.exe') $binDir
  # Remove the two retired public command names in this installation.
  Remove-Item -Force -ErrorAction SilentlyContinue (Join-Path $binDir 'wonder.exe'), (Join-Path $binDir 'wonder-host.exe')
  foreach ($runtime in $staged.Keys) {
    $target = Join-Path $share $runtime
    if (Test-Path $target) { Remove-Item -Recurse -Force $target }
    Move-Item $staged[$runtime] $target
  }
  $staged = @{}
} finally {
  foreach ($stage in $staged.Values) { Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $stage }
}

$helper = Join-Path $share 'computer-windows'
New-Item -ItemType Directory -Force $helper | Out-Null
Copy-Item -Force (Join-Path $repo 'native\computer-use-windows\*.py'), (Join-Path $repo 'native\computer-use-windows\requirements.txt') $helper
Copy-Item -Force (Join-Path $repo 'compatibility-manifest.json') $share
if ($venvPython) {
  [Environment]::SetEnvironmentVariable('WONDER_COMPUTER_PYTHON', $venvPython, 'User')
  $env:WONDER_COMPUTER_PYTHON = $venvPython
}

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($userPath -split ';') -notcontains $binDir) {
  [Environment]::SetEnvironmentVariable('Path', "$binDir;$userPath", 'User')
}
$wonder = Join-Path $binDir 'cupola.exe'
# The data directory `cupola` uses: WONDER_DATA_DIR (relative to here, as
# cupola.exe resolves it) or %LOCALAPPDATA%\Wonder.
$dataDir = if ($env:WONDER_DATA_DIR) {
  [IO.Path]::GetFullPath([IO.Path]::Combine((Get-Location).ProviderPath, $env:WONDER_DATA_DIR))
} else { Join-Path $env:LOCALAPPDATA 'Wonder' }
$initialized = Test-Path (Join-Path $dataDir 'headless\config.json')
if (-not $NoService) {
  Invoke-Native 'cupola service install' { & $wonder service install | Out-Null }
  Write-Output 'The Cupola host starts when you sign in to Windows.'
}
Write-Output "Installed in $Prefix (bin is on your PATH for new terminals)."
if ($initialized -and -not $NoStart) { Invoke-Native 'cupola start' { & $wonder start } }
if (-not $initialized) {
  Write-Output 'Next: cupola init (Tailscale signed in first), then cupola start and cupola pair.'
}

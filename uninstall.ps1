# Remove only this receipt-owned installation; never delete data or sign-ins.
$ErrorActionPreference = 'Stop'
$prefix = $PSScriptRoot
$receipt = Get-Content -Raw (Join-Path $prefix 'install.json') | ConvertFrom-Json
if ($receipt.owner -ne 'wonder-cli-v1' -or $receipt.prefix -ne $prefix -or ((Get-Item $prefix).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
  throw 'Invalid Cupola installation receipt; nothing removed.'
}
$command = Join-Path $prefix 'bin\cupola.cmd'
$data = if ($env:WONDER_DATA_DIR) { $env:WONDER_DATA_DIR } else { Join-Path $env:LOCALAPPDATA 'Wonder' }
$config = Join-Path $data 'headless\config.json'
if (-not (Test-Path $config) -and (Test-Path (Join-Path $data 'headless\supervisor.pid'))) { throw 'A host PID remains without configuration. Stop that host before removing Cupola.' }
if (-not (Test-Path $command) -and (Test-Path $config)) { throw 'Could not locate the installed CLI to check the host. Restore its launcher before uninstalling.' }
if ((Test-Path $command) -and (Test-Path $config)) {
  $status = & $command status --json
  if ($LASTEXITCODE -ne 0) { throw 'Could not check the host. Stop it and uninstall its service before removing Cupola.' }
  $status = $status | ConvertFrom-Json
  if ($status.running -or $status.supervisorPid) {
    $name = [IO.Path]::GetFileNameWithoutExtension($command)
    throw "Cupola is running. Run $name stop and $name service uninstall first, then retry."
  }
}
if ($receipt.addedPath) {
  $path = [Environment]::GetEnvironmentVariable('Path', 'User')
  [Environment]::SetEnvironmentVariable('Path', (($path -split ';' | Where-Object { $_ -ne $receipt.binDir }) -join ';'), 'User')
}
Remove-Item -Recurse -Force $prefix
Write-Output 'Cupola uninstalled. Conversations, pairing, settings and provider sign-ins were kept.'

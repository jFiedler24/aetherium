# Build aetherium for Windows and package a zip in dist\.
# Usage: .\scripts\build-windows.ps1 [-Profile debug]   (default: release)
param(
    [ValidateSet('debug', 'release')]
    [string]$Profile = 'release'
)
$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot
Set-Location $Root

$args = @('build', '--locked')
if ($Profile -eq 'release') { $args += '--release' }
& cargo @args
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"' |
    Select-Object -First 1).Matches.Groups[1].Value
$exe = Join-Path $Root "target\$Profile\aetherium.exe"
if (-not (Test-Path $exe)) { throw "expected binary not found: $exe" }

$stage = Join-Path $Root 'dist\aetherium-windows-x86_64'
Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item $exe $stage
Copy-Item (Join-Path $Root 'README.md') $stage

$zip = Join-Path $Root "dist\aetherium-$version-windows-x86_64.zip"
Compress-Archive -Path "$stage\*" -DestinationPath $zip -Force
Write-Host "built $zip ($Profile)"

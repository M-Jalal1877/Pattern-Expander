# Builds the optimised application and puts it in .\dist as
# "Acme Engineering Hatching.exe" (Cargo itself cannot use spaces in a binary name).
#
# Usage:  powershell -ExecutionPolicy Bypass -File .\build-release.ps1
$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

cargo build --release
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

New-Item -ItemType Directory -Force "dist" | Out-Null
$out = Join-Path "dist" "Acme Engineering Hatching.exe"
Copy-Item "target\release\Acme-Engineering-Hatching.exe" $out -Force
Write-Host "Built: $((Resolve-Path $out).Path)"

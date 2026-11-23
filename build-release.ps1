# Builds the optimised application into .\dist, ready to copy to another PC:
#
#   dist\
#     Acme Engineering Hatching.exe
#     patterns\            <- your saved patterns, one .json file each
#
# Copy the whole dist folder; the program reads and saves patterns in the
# "patterns" folder next to it. (Cargo itself cannot use spaces in a binary name,
# hence the copy to "Acme Engineering Hatching.exe".)
#
# Usage:  powershell -ExecutionPolicy Bypass -File .\build-release.ps1
$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

cargo build --release
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

New-Item -ItemType Directory -Force "dist" | Out-Null
$exe = Join-Path "dist" "Acme Engineering Hatching.exe"
Copy-Item "target\release\Acme-Engineering-Hatching.exe" $exe -Force

# Bring along patterns saved while running from the build folders
# (cargo run / target\release), without overwriting patterns already in dist.
$patterns = Join-Path "dist" "patterns"
New-Item -ItemType Directory -Force $patterns | Out-Null
foreach ($src in @("target\debug\patterns", "target\release\patterns")) {
    if (Test-Path $src) {
        Get-ChildItem $src -Filter *.json | ForEach-Object {
            $dst = Join-Path $patterns $_.Name
            if (-not (Test-Path $dst)) { Copy-Item $_.FullName $dst }
        }
    }
}

$n = (Get-ChildItem $patterns -Filter *.json).Count
Write-Host "Built: $((Resolve-Path $exe).Path)"
Write-Host "Patterns: $n file(s) in $((Resolve-Path $patterns).Path)"

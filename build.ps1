# build.ps1 - Build latest release + update portable
# Usage: run from repo root  .\build.ps1
# After changing code (Rust or src/ frontend), run this to refresh
# release\ovoice-portable\ovoice.exe + mem.exe to the latest.
#
# Flow:
#   1. cargo build --release --bin ovoice --bin mem  (compile Rust + embed src/ via generate_context!)
#   2. stop portable ovoice if running (release the .exe lock)
#   3. copy target/release/{ovoice,mem}.exe -> release/ovoice-portable/

$ErrorActionPreference = "Stop"

$ROOT   = Split-Path -Parent $MyInvocation.MyCommand.Path
$SRC    = Join-Path $ROOT "src-tauri"
$TARGET = Join-Path $SRC  "target\release"
$PORT   = Join-Path $ROOT "release\ovoice-portable"

Write-Host ""
Write-Host "[1/3] cargo build --release --bin ovoice --bin mem" -ForegroundColor Cyan
cargo build --manifest-path "$SRC\Cargo.toml" --release --bin ovoice --bin mem
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

Write-Host ""
Write-Host "[2/3] verify exes" -ForegroundColor Cyan
foreach ($e in @("ovoice.exe", "mem.exe")) {
    $p = Join-Path $TARGET $e
    if (-not (Test-Path $p)) { throw "$e not found: $p" }
    Write-Host "  OK $e  $((Get-Item $p).LastWriteTime)" -ForegroundColor Green
}

Write-Host ""
Write-Host "[3/3] copy to portable: $PORT" -ForegroundColor Cyan
if (Get-Process ovoice -ErrorAction SilentlyContinue) {
    Write-Host "  stopping running ovoice.exe (release lock)..." -ForegroundColor Yellow
    Stop-Process -Name ovoice -Force -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 1
}
New-Item -ItemType Directory -Force -Path $PORT | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $PORT "assets") | Out-Null
try {
    Copy-Item "$TARGET\ovoice.exe" "$PORT\ovoice.exe" -Force
    Copy-Item "$TARGET\mem.exe"   "$PORT\mem.exe"   -Force
} catch {
    throw "copy failed - ovoice.exe still locked. Run taskkill /F /IM ovoice.exe then retry."
}

Write-Host ""
Write-Host "[4/4] bundle tuned preset (bg + config.preset.json)" -ForegroundColor Cyan
Copy-Item "$ROOTesources\presetg.jpg" (Join-Path $PORT "assetsg.jpg") -Force
Copy-Item "$ROOTesources\preset\config.preset.json" "$PORT\config.preset.json" -Force
Write-Host "  OK assetsg.jpg + config.preset.json (first-run seed; existing users unaffected)" -ForegroundColor Green

Write-Host ""
Write-Host "DONE - portable updated to latest:" -ForegroundColor Green
Get-ChildItem $PORT -File | Format-Table Name, @{N='MB'; E={[math]::Round($_.Length / 1MB, 1)}}, LastWriteTime -AutoSize
Write-Host "Run $PORT\ovoice.exe to verify." -ForegroundColor Yellow
Write-Host ""

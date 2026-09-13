# build.ps1 - Build latest release + update portable
# Usage: run from repo root  .\build.ps1
# After changing code (Rust or src/ frontend), run this to refresh
# release\ovoice-portable\ovoice.exe + mem.exe to the latest.
#
# Flow:
#   1. cargo build --release --bin ovoice --bin mem  (compile Rust + embed src/ via generate_context!)
#   2. stop portable ovoice if running (release the .exe lock)
#   3. copy target/release/{ovoice,mem}.exe -> release/ovoice-portable/
#   4. bundle tuned preset (assets/bg.jpg + config.preset.json, first-run seed)
#   5. bundle busybox64u.exe + LICENSE + SHA256SUM

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
Write-Host "[4/5] bundle tuned preset (bg + config.preset.json)" -ForegroundColor Cyan
Copy-Item (Join-Path $ROOT "resources\preset\bg.jpg") (Join-Path $PORT "assets\bg.jpg") -Force
Copy-Item (Join-Path $ROOT "resources\preset\config.preset.json") (Join-Path $PORT "config.preset.json") -Force
Write-Host "  OK assets\bg.jpg + config.preset.json (first-run seed; existing users unaffected)" -ForegroundColor Green

Write-Host ""
Write-Host "[5/5] bundle busybox + LICENSE + SHA256SUM" -ForegroundColor Cyan
# busybox64u.exe 因 GPL 采购与体积不随 git 仓库分发；本机 src-tauri/binaries/ 应已就位
$BUSY = Join-Path $SRC "binaries\busybox64u.exe"
if (Test-Path $BUSY) {
    Copy-Item $BUSY (Join-Path $PORT "busybox64u.exe") -Force
    Write-Host "  OK busybox64u.exe" -ForegroundColor Green
} else {
    Write-Host "  WARN busybox64u.exe missing - bash tool will fall back to PATH." -ForegroundColor Yellow
    Write-Host "       Download from https://frippery.org/busybox/ into src-tauri/binaries/ and rerun." -ForegroundColor Yellow
}
Copy-Item (Join-Path $ROOT "LICENSE") (Join-Path $PORT "LICENSE") -Force
# 生成 SHA256SUM（sha256sum 十六进制 + 两个空格 + 文件名，兼容 coreutils 校验）
$hashLines = Get-ChildItem $PORT -File | ForEach-Object {
    "{0}  {1}" -f (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower(), $_.Name
}
$hashLines | Set-Content (Join-Path $PORT "SHA256SUM") -Encoding Ascii
Write-Host "  OK LICENSE + SHA256SUM" -ForegroundColor Green

Write-Host ""
Write-Host "DONE - portable updated to latest:" -ForegroundColor Green
Get-ChildItem $PORT -File | Format-Table Name, @{N='MB'; E={[math]::Round($_.Length / 1MB, 1)}}, LastWriteTime -AutoSize
Write-Host "Run $PORT\ovoice.exe to verify." -ForegroundColor Yellow
Write-Host ""

# DTM 配布用インストーラのビルド
# 使い方: powershell -ExecutionPolicy Bypass -File installer\build.ps1 [-Version 0.4.12] [-SkipBuild]
param(
  [string]$Version = "",
  [switch]$SkipBuild
)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.Encoding]::UTF8

$root = Split-Path $PSScriptRoot -Parent
$iss  = Join-Path $PSScriptRoot 'setup.iss'
$pay  = Join-Path $PSScriptRoot 'payload'
$dist = Join-Path $PSScriptRoot 'dist'

# 版: 引数 > extension/package.json
if (-not $Version) {
  $pkgRaw = Get-Content (Join-Path $root 'extension\package.json') -Raw
  if ($pkgRaw -match '"version"\s*:\s*"([^"]+)"') { $Version = $Matches[1] }
  else { throw "extension/package.json から version を取得できません" }
}
$vsixName = "devin-tool-manager-$Version.vsix"
Write-Output "== bundle version: $Version"

# dtm.exe
if (-not $SkipBuild) {
  Write-Output "== cargo build --release"
  Push-Location $root
  cargo build --release
  if ($LASTEXITCODE -ne 0) { Pop-Location; throw "cargo build failed" }
  Pop-Location
}
$dtmExe = Join-Path $root 'target\release\dtm.exe'
if (-not (Test-Path $dtmExe)) { throw "dtm.exe not found: $dtmExe" }

# VSIX
$vsix = Join-Path $root $vsixName
python (Join-Path $PSScriptRoot 'pack_vsix.py') $vsix
if (-not (Test-Path $vsix)) { throw "vsix pack failed" }

# payload ステージング
New-Item -ItemType Directory -Force $pay, $dist | Out-Null
Copy-Item $dtmExe (Join-Path $pay 'dtm.exe') -Force
Copy-Item $vsix (Join-Path $pay $vsixName) -Force
Set-Content -Path (Join-Path $pay 'dtm.root') -Value "dtm root marker`n" -NoNewline -Encoding ASCII

# ISCC
$iscc = "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
if (-not (Test-Path $iscc)) { $iscc = "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe" }
if (-not (Test-Path $iscc)) { throw "ISCC.exe not found (Inno Setup 6 未導入)" }
& $iscc "/DAppVersion=$Version" "/DVsixFile=$vsixName" $iss
if ($LASTEXITCODE -ne 0) { throw "ISCC failed" }
Get-ChildItem $dist -Filter *.exe | ForEach-Object { Write-Output ("== built: " + $_.FullName) }

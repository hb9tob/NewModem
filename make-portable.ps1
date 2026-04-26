#requires -Version 5.1
<#
.SYNOPSIS
  Construit un zip portable Windows du modem NBFM.

.DESCRIPTION
  Build CLI + GUI release, puis assemble dans dist/portable/ un dossier
  pret a zipper contenant :
    - nbfm-modem-gui.exe                              (binaire GUI)
    - nbfm-modem-x86_64-pc-windows-msvc.exe           (sidecar CLI)
    - portable.txt                                    (marqueur mode portable)
    - README-portable.txt                             (notice utilisateur)

  Le marqueur portable.txt declenche le helper Rust portable_root() qui
  redirige settings, captures RX et sessions dans <exe_dir>/data/.

  Le secret HMAC du collecteur est embarque dans le binaire au build
  (include_str! sur secret.txt). Le zip publie partage donc ce secret
  avec le collector serveur, ce qui permet a tout porteur du zip
  d'envoyer des sondages de canal.

.PARAMETER Tag
  Suffixe du nom de zip. Par defaut : sortie de `git describe --tags --always`.

.PARAMETER SkipBuild
  Saute les `cargo build` (utile si on vient juste de builder).

.EXAMPLE
  .\make-portable.ps1
  .\make-portable.ps1 -Tag v0.1.0-portable-test
  .\make-portable.ps1 -SkipBuild
#>
[CmdletBinding()]
param(
    [string]$Tag,
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'

$RepoRoot   = Split-Path -Parent $MyInvocation.MyCommand.Path
$RustDir    = Join-Path $RepoRoot 'rust'
$TargetDir  = Join-Path $RustDir  'target\release'
$DistRoot   = Join-Path $RepoRoot 'dist\portable'

if (-not (Test-Path $RustDir)) {
    throw "rust\ introuvable sous $RepoRoot"
}

if (-not $Tag) {
    Push-Location $RepoRoot
    try {
        $Tag = (git describe --tags --always 2>$null).Trim()
        if (-not $Tag) { $Tag = 'dev' }
    } finally { Pop-Location }
}

$Triple    = 'x86_64-pc-windows-msvc'
$StageName = "nbfm-modem-portable-$Tag"
$StageDir  = Join-Path $DistRoot $StageName
$ZipPath   = Join-Path $DistRoot "$StageName.zip"

Write-Host "[portable] tag       = $Tag"
Write-Host "[portable] staging   = $StageDir"
Write-Host "[portable] zip       = $ZipPath"

if (-not $SkipBuild) {
    Write-Host "[portable] cargo build --release -p modem-cli"
    Push-Location $RustDir
    try {
        & cargo build --release -p modem-cli
        if ($LASTEXITCODE -ne 0) { throw "cargo build modem-cli a echoue" }
        Write-Host "[portable] cargo build --release -p modem-gui"
        & cargo build --release -p modem-gui
        if ($LASTEXITCODE -ne 0) { throw "cargo build modem-gui a echoue" }
    } finally { Pop-Location }
} else {
    Write-Host "[portable] -SkipBuild : saute les cargo build"
}

$GuiExe = Join-Path $TargetDir 'nbfm-modem-gui.exe'
$CliExe = Join-Path $TargetDir 'nbfm-modem.exe'
foreach ($p in @($GuiExe, $CliExe)) {
    if (-not (Test-Path $p)) { throw "Binaire absent : $p" }
}

if (Test-Path $StageDir) {
    Remove-Item $StageDir -Recurse -Force
}
New-Item -ItemType Directory -Path $StageDir -Force | Out-Null

Copy-Item $GuiExe (Join-Path $StageDir 'nbfm-modem-gui.exe')
Copy-Item $CliExe (Join-Path $StageDir "nbfm-modem-$Triple.exe")

# Marqueur portable : presence suffit, contenu ignore par le code Rust.
Set-Content -Path (Join-Path $StageDir 'portable.txt') -Value '' -Encoding ascii -NoNewline

$ReadmeSrc = Join-Path $RepoRoot 'rust\modem-gui\portable\README-portable.txt'
if (Test-Path $ReadmeSrc) {
    Copy-Item $ReadmeSrc (Join-Path $StageDir 'README-portable.txt')
} else {
    Write-Warning "README-portable.txt introuvable a $ReadmeSrc — zip livre sans notice."
}

if (Test-Path $ZipPath) { Remove-Item $ZipPath -Force }
Compress-Archive -Path (Join-Path $StageDir '*') -DestinationPath $ZipPath -CompressionLevel Optimal

$ZipInfo = Get-Item $ZipPath
$Size    = [math]::Round($ZipInfo.Length / 1MB, 2)
Write-Host ""
Write-Host "[portable] OK"
Write-Host "[portable] $($ZipInfo.FullName)  ($Size MB)"

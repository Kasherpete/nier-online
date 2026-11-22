# Builds nier-online and installs it into the game folder as dinput8.dll.
#
# Usage:
#   tools\install.ps1                 build (release) and install
#   tools\install.ps1 -NoBuild        install the last build
#   tools\install.ps1 -Uninstall      remove it (restores a dinput8.dll that was there before)
#   tools\install.ps1 -GameDir <path> if the game is not in the default Steam folder
param(
    [string]$GameDir = "C:\Program Files (x86)\Steam\steamapps\common\NieRAutomata",
    [switch]$NoBuild,
    [switch]$Uninstall
)

$ErrorActionPreference = "Stop"
$Root = Split-Path $PSScriptRoot -Parent
$Dest = Join-Path $GameDir "dinput8.dll"
$Backup = Join-Path $GameDir "dinput8.dll.bak"

if (-not (Test-Path (Join-Path $GameDir "NieRAutomata.exe"))) {
    throw "NieRAutomata.exe not found in $GameDir. Pass -GameDir <game folder>."
}

# Our DLL contains this string; anything else named dinput8.dll belongs to another mod.
function Test-Ours([string]$path) {
    $text = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($path))
    return $text.Contains("nier_online.toml")
}

if ($Uninstall) {
    if ((Test-Path $Dest) -and (Test-Ours $Dest)) {
        Remove-Item $Dest
        Write-Host "Removed nier-online."
        if (Test-Path $Backup) {
            Move-Item $Backup $Dest
            Write-Host "Restored the previous dinput8.dll."
        }
    } else {
        Write-Host "nier-online is not installed in $GameDir."
    }
    exit 0
}

if (-not $NoBuild) {
    Push-Location $Root
    try {
        cargo build --release
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
    } finally {
        Pop-Location
    }
}

$Dll = Join-Path $Root "target\release\nier_online.dll"
if (-not (Test-Path $Dll)) { throw "$Dll not found. Run without -NoBuild." }

if ((Test-Path $Dest) -and -not (Test-Ours $Dest)) {
    if (Test-Path $Backup) { throw "$Dest belongs to another mod and $Backup already exists. Move one of them first." }
    Move-Item $Dest $Backup
    Write-Host "Moved the existing dinput8.dll (another mod) to dinput8.dll.bak."
}

Copy-Item $Dll $Dest -Force
Write-Host "Installed nier-online to $Dest"
Write-Host "Start the game and press Insert to open the menu."

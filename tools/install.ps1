# Installs nier-online into the game folder as dinput8.dll.
#
# From a downloaded release (dinput8.dll next to this script): installs that DLL.
# From a source checkout (tools\install.ps1): builds a release DLL first.
#
# Usage:
#   install.ps1                 install (building first in a source checkout)
#   install.ps1 -NoBuild        source checkout: install the last build
#   install.ps1 -Uninstall      remove it (restores a dinput8.dll that was there before)
#   install.ps1 -GameDir <path> if the game folder is not found automatically
#   install.ps1 -Yes            source checkout: install missing build tools without asking
#
# In a source checkout it first checks for Rust (rustup) and the Visual Studio C++ build tools, and offers to
# install whichever is missing with winget.
param(
    [string]$GameDir = "",
    [switch]$NoBuild,
    [switch]$Uninstall,
    [switch]$Yes
)

$ErrorActionPreference = "Stop"

# The game folder: the default Steam library, then every library listed in Steam's libraryfolders.vdf.
function Find-GameDir {
    $candidates = @("C:\Program Files (x86)\Steam\steamapps\common\NieRAutomata")
    $steam = (Get-ItemProperty -Path "HKCU:\Software\Valve\Steam" -ErrorAction SilentlyContinue).SteamPath
    if ($steam) {
        $vdf = Join-Path $steam "steamapps\libraryfolders.vdf"
        if (Test-Path $vdf) {
            foreach ($m in [regex]::Matches((Get-Content $vdf -Raw), '"path"\s+"([^"]+)"')) {
                $candidates += Join-Path ($m.Groups[1].Value -replace '\\\\', '\') "steamapps\common\NieRAutomata"
            }
        }
    }
    foreach ($c in $candidates) {
        if (Test-Path (Join-Path $c "NieRAutomata.exe")) { return $c }
    }
    return $null
}

# --- Build tools (source checkouts only) ---------------------------------------------------------------

$CargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
$VsWhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"

function Test-Rust {
    if (Get-Command rustup -ErrorAction SilentlyContinue) { return $true }
    # Installed but not on this session's PATH yet (fresh install, or a shell opened before it).
    if (Test-Path (Join-Path $CargoBin "rustup.exe")) {
        $env:Path = "$CargoBin;$env:Path"
        return $true
    }
    return $false
}

# MSVC's C++ compiler and linker (link.exe), from any Visual Studio edition or the standalone Build Tools.
function Test-MsvcTools {
    if (-not (Test-Path $VsWhere)) { return $false }
    $found = & $VsWhere -products * -latest -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    return [bool]$found
}

function Confirm-Install([string]$what) {
    if ($Yes) { return $true }
    $answer = Read-Host "$what is needed to build nier-online but is not installed. Install it now with winget? [Y/n]"
    return ($answer -eq "" -or $answer -match '^[Yy]')
}

function Install-WithWinget([string]$id, [string[]]$extra) {
    if (-not (Get-Command winget -ErrorAction SilentlyContinue)) {
        throw "winget is not available. Install $id by hand (see the README's Building section), then run this again."
    }
    & winget install --id $id -e --accept-source-agreements --accept-package-agreements @extra
    if ($LASTEXITCODE -ne 0) { throw "winget could not install $id (exit code $LASTEXITCODE)." }
}

function Assert-BuildTools {
    if (-not (Test-Rust)) {
        if (-not (Confirm-Install "Rust (rustup)")) { throw "Rust is required. Install it from https://rustup.rs and run this again." }
        Install-WithWinget "Rustlang.Rustup" @()
        if (-not (Test-Rust)) { throw "Rust was installed but rustup was not found. Open a new terminal and run this again." }
        Write-Host "Installed Rust."
    }
    if (-not (Test-MsvcTools)) {
        if (-not (Confirm-Install "The Visual Studio C++ build tools (about 2-3 GB)")) {
            throw "The C++ build tools are required. Install 'Desktop development with C++' from https://visualstudio.microsoft.com/visual-cpp-build-tools/ and run this again."
        }
        Write-Host "Installing the Visual Studio C++ build tools. Windows will ask for administrator permission; this takes several minutes."
        Install-WithWinget "Microsoft.VisualStudio.2022.BuildTools" @(
            "--override", "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
        )
        if (-not (Test-MsvcTools)) { throw "The C++ build tools did not finish installing. Run the Visual Studio Installer, add 'Desktop development with C++', then run this again." }
        Write-Host "Installed the C++ build tools."
    }
}

# --- Game folder -----------------------------------------------------------------------------------------

if (-not $GameDir) { $GameDir = Find-GameDir }
if (-not $GameDir -or -not (Test-Path (Join-Path $GameDir "NieRAutomata.exe"))) {
    throw "NieRAutomata.exe not found. Pass -GameDir <game folder> (the folder that holds NieRAutomata.exe)."
}
$Dest = Join-Path $GameDir "dinput8.dll"
$Backup = Join-Path $GameDir "dinput8.dll.bak"
$Packaged = Join-Path $PSScriptRoot "dinput8.dll"

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

if (Test-Path $Packaged) {
    $Dll = $Packaged # a downloaded release
} else {
    $Root = Split-Path $PSScriptRoot -Parent
    if (-not $NoBuild) {
        Assert-BuildTools
        Push-Location $Root
        try {
            # Installs the toolchain rust-toolchain.toml pins, if missing (a no-op when it is there). Needs
            # rustup 1.28+; older ones fail here but install it on the first cargo call instead.
            rustup toolchain install
            cargo build --release
            if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
        } finally {
            Pop-Location
        }
    }
    $Dll = Join-Path $Root "target\release\nier_online.dll"
    if (-not (Test-Path $Dll)) { throw "$Dll not found. Run without -NoBuild." }
}

if ((Test-Path $Dest) -and -not (Test-Ours $Dest)) {
    if (Test-Path $Backup) { throw "$Dest belongs to another mod and $Backup already exists. Move one of them first." }
    Move-Item $Dest $Backup
    Write-Host "Moved the existing dinput8.dll (another mod) to dinput8.dll.bak."
}

try {
    Copy-Item $Dll $Dest -Force
} catch {
    throw "Could not write $Dest. Close the game first. ($_)"
}
Write-Host "Installed nier-online to $Dest"
Write-Host "Start the game and press Insert to open the menu."

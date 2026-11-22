# Starts two copies of NieR:Automata on this PC, for local two-instance testing of nier-online.
#
# How it works:
#   - Steam itself allows the second copy. Launching the exe directly with SteamAppId set skips Steam's
#     "already running" check, and the game does not call SteamAPI_RestartAppIfNecessary.
#   - The game refuses to start twice ("ERROR: Game is already running.") because of a named event,
#     "NieR:AutomataEvent" (verified on Steam build Last_SVN_REVISION=7788). After the first copy creates
#     it, this script closes that handle inside the first copy, then starts the second. (The mutex
#     CDdf212806D6EmB31yE0c is NOT the lock; closing it alone does nothing.)
#
# Usage: launch-two.ps1 [-GameDir <path>] [-Delay <seconds between the two launches>]
param(
    [string]$GameDir = "C:\Program Files (x86)\Steam\steamapps\common\NieRAutomata",
    [int]$Delay = 3
)

$ErrorActionPreference = "Stop"
$LockName = "NieR:AutomataEvent"
$AppId = "524220"
$Exe = Join-Path $GameDir "NieRAutomata.exe"

function Fail($msg) {
    Write-Host "ERROR: $msg" -ForegroundColor Red
    Read-Host "Press Enter to close"
    exit 1
}

if (-not (Test-Path $Exe)) { Fail "NieRAutomata.exe not found in $GameDir (pass -GameDir)." }
Add-Type -Path (Join-Path $PSScriptRoot "MutexTool.cs")

# 1. Steam must be running and logged in, or SteamAPI_Init fails in the game.
function Get-SteamUser { (Get-ItemProperty "HKCU:\Software\Valve\Steam\ActiveProcess" -ErrorAction SilentlyContinue).ActiveUser }
if (-not (Get-SteamUser)) {
    $steam = (Get-ItemProperty "HKCU:\Software\Valve\Steam" -ErrorAction SilentlyContinue).SteamExe
    if (-not $steam) { $steam = "C:\Program Files (x86)\Steam\steam.exe" }
    Write-Host "Starting Steam..."
    Start-Process $steam -ArgumentList "-silent"
    $deadline = (Get-Date).AddSeconds(120)
    while (-not (Get-SteamUser)) {
        if ((Get-Date) -gt $deadline) { Fail "Steam did not log in within 2 minutes." }
        Start-Sleep -Seconds 2
    }
    Start-Sleep -Seconds 10  # Let Steam finish starting its services.
}

$env:SteamAppId = $AppId
$env:SteamGameId = $AppId

function Close-GameLock([int]$procId) {
    $closed = [MutexTool]::Named($procId, $LockName, $true, [string[]]@("Event"))
    return ($closed | Where-Object { $_ -like "*(closed)" }).Count -gt 0
}

# 2. First copy. If one is already running (started from Steam, say), reuse it as copy A.
$a = Get-Process NieRAutomata -ErrorAction SilentlyContinue | Select-Object -First 1
if ($a) {
    Write-Host "Copy A is already running (pid $($a.Id))."
} else {
    $a = Start-Process $Exe -WorkingDirectory $GameDir -PassThru
    Write-Host "Started copy A (pid $($a.Id))."
}

# 3. Wait for copy A to create its lock, then close it. If copy A already lost it (closed on an earlier
#    run), the timeout below is harmless: the second copy will start anyway.
$deadline = (Get-Date).AddSeconds(60)
$done = $false
while (-not $done -and (Get-Date) -lt $deadline) {
    if ($a.HasExited) { Fail "Copy A exited during startup (exit code $($a.ExitCode))." }
    $done = Close-GameLock $a.Id
    if (-not $done) { Start-Sleep -Milliseconds 500 }
}
if ($done) { Write-Host "Closed the single-instance lock in copy A." }
else { Write-Host "Lock not found in copy A after 60 s; trying copy B anyway." -ForegroundColor Yellow }

# 4. Second copy.
Start-Sleep -Seconds $Delay
$b = Start-Process $Exe -WorkingDirectory $GameDir -PassThru
Write-Host "Started copy B (pid $($b.Id))."
Start-Sleep -Seconds 2

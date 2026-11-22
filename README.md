# nier-online

Online peer-to-peer co-op for NieR:Automata (Steam, Windows). A DLL loaded into the game spawns the other
player as a second character in your world. One player hosts: their game runs the enemies, quests and
cutscenes, and the other player's game follows along.

> **Status: early development.** Joining by code, the in-game menu, player sync, the name tag and enemy sync
> work in-game. Boss fights, enemy deaths and damage across both games are still being tested. Areas,
> quests, cutscenes and saves are not synced yet, so stay close together and play the same part of the
> story. See [CLAUDE.md](CLAUDE.md) for the roadmap.

## Goals

- **Two-player co-op** through the open world, combat and (eventually) the story.
- **Peer-to-peer.** No dedicated server, no port forwarding and no VPN. The host shares a join code, and the
  connection gets through home routers automatically, falling back to a relay when a direct path is impossible.
- **In-game menu** (ImGui overlay) for hosting, joining and status, so you never have to edit config files.
- **Host-authoritative world.** The host's game owns enemies, quests, cutscenes and saves; each player owns
  their own character.

## Features

| Area | Status |
|---|---|
| Join codes, direct or relayed connection (iroh), version check | Works |
| In-game menu: host / join / status / ping / chat / settings / log | Works |
| Player sync (movement, attacks, pod fire and programs, flashlight) | Works |
| Name tag over the other player | Works |
| Enemy sync (the host's enemies decide, the client's follow) | Works (experimental) |
| Enemy deaths, damage and boss fights across both games | Being tested |
| Area transitions follow the host | Planned |
| Quest, cutscene and save sync | Planned |
| Flight-unit, hacking and 2D sections | Planned |

## Requirements

- NieR:Automata, Steam version, on Windows.
- Both players need the same game version and the same build of this mod. The menu's title shows the build
  (`nier-online 0.1.0+<commit>`), and joining with a different build is refused with a message saying so.

## Installing

1. Download `nier-online.zip` from the [latest build](https://github.com/Kasherpete/nier-online/releases/tag/latest).
2. Unzip it, right-click `install.ps1` and pick **Run with PowerShell**. It finds the game folder in your Steam
   libraries and copies `dinput8.dll` next to `NieRAutomata.exe` (a `dinput8.dll` from another mod is kept as
   `dinput8.dll.bak`). Or copy `dinput8.dll` there yourself.
3. Start the game and press **Insert** to open the menu.
4. Host: click **Host**, then **Copy join code**, and send the code to your friend.
   Client: paste the code and click **Join**.

To uninstall, run `install.ps1 -Uninstall` or delete `dinput8.dll` from the game folder. If the game is not
found, pass its folder: `install.ps1 -GameDir "D:\Games\Steam\steamapps\common\NieRAutomata"`.

If PowerShell refuses to run the script (*"running scripts is disabled on this system"*, or a security
warning because it was downloaded), run it once with
`powershell -ExecutionPolicy Bypass -File install.ps1` from the unzipped folder.

**Testing alone:** the **Mirror test** button makes a copy of you follow 2 m to the side, driven by the same
code that drives a real second player.

## Enemies and bosses

Enemy sync is on by default; the host's **Enemy sync** setting decides for both players.

- The host's game decides what every enemy does. The client's enemies stand where the host's stand, play the
  same attacks, and show the same HP.
- Where both games spawned the same enemy (same type, nearby), the client keeps its own and makes it follow the
  host's. A boss (the only enemy of its type) pairs at any distance, so the client's boss scripts and boss
  parts stay intact. Enemies only the host has are spawned on the client; enemies only the client has are
  removed after a few seconds.
- The client's hits reach the host through the client's character in the host's game, and an enemy dies on
  both sides when it dies for the host.
- **Replay all enemy animations** (client setting, on by default) plays every enemy move the host sends. If
  enemy fights crash the client, turn it off: enemies then only play moves the client's game has already
  used for that enemy type, so they may slide around until those are learned.

Without quest sync, play the same mission at the same place: a boss that only one game has started cannot
be paired.

## Settings

Saved by the menu's **Settings** tab to `nier_online.toml` next to the exe: player name, menu key, enemy
sync, enemy animation replay, relay URL, log level, and the debug panel. The log is written to
`nier_online.log` in the same folder (`nier_online.2.log` for a second copy of the game running at once).

## Contributing

Issues and pull requests are welcome. Read [CLAUDE.md](CLAUDE.md) first: it holds the roadmap, the
architecture, and the rules for reversed structs and pointer safety.

## License

[GPL-3.0](LICENSE). This project builds on nier_coop, which is GPL-3.0, so it must stay GPL-3.0.
Third-party notices are in [LICENSE-THIRD-PARTY](LICENSE-THIRD-PARTY).

## Credits

- [rainindaily/nier_coop](https://github.com/rainindaily/nier_coop) (GPL-3.0): partner-spawn method, remote
  player driving, pod sync, the seqlock state handoff, the pattern scanner, and pairing the client's own
  enemies with the host's while blocking their AI.
- [praydog/AutomataMP](https://github.com/praydog/AutomataMP) (MIT): original offsets and patterns, the
  host-owned enemy sync design, the ImGui overlay approach, and the reversed engine structures.

**0.2.1:** Fixed mouse being reset to center while in nier-online ImGui menu, minor enhancements

**v0.2.0:** Added enemy position and state synchronization, updated networking implementation, fixed various minor bugs.

**v0.1.0:** Initial commit. Implemented player state synchronization and ImGui menu.
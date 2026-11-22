# nier-online

Peer-to-peer two-player co-op mod for NieR:Automata, built as a Rust DLL that runs inside the game process.
It is a new project based on two earlier mods:

- **nier_coop** (Rust, GPL-3.0): https://github.com/rainindaily/nier_coop. Port its code directly where it fits.
- **AutomataMP** (C++, MIT): https://github.com/praydog/AutomataMP. Use it as a reference: port its designs
  and reversed structures to Rust, and keep its MIT notice with any ported code.

This file is the roadmap. Work through the phases in order: each phase depends on the ones before it.
Tick boxes as items are done and verified in-game, not when the code merely compiles.

## Target binary

- `NieRAutomata.exe`: x86_64 Windows, MSVC ABI, PlatinumGames' own C++ engine, DirectX 11.
- Calling convention is x64 MSVC (`__fastcall`). Hooked functions use `extern "system"`; `this` is the first
  argument (RCX).
- ASLR is on. **Never hardcode absolute addresses.** Resolve every address by pattern scan, or as
  `module_base + offset` with the offset documented next to the game build it came from.
- The game still gets Steam updates. Every address lookup must fail loudly and safely when a pattern
  stops matching.

## Engine terminology

- Entities inherit from `BehaviorAppBase` / `Behavior`. Enemies are `EmBase` subclasses.
- Playable characters (2B, 9S, A2) are class `Pl0000`.
- The companion runs through the buddy / NPC behavior routines.
- World and entity management: `SceneEntitySystem`, `BehaviorSystem`, the entity list, and `EntityLayout`.
- The game has an embedded script VM (mruby). Script functions are registered in a table that can be found
  through strings such as `EntityLayout.enableCreateEntity` (see AutomataMP `ScriptFunctions.cpp`).

## Architecture

```
 game thread (hooks)                 net thread (tokio + iroh)                peer
 ───────────────────                 ─────────────────────────                ────
 per-frame update hook ──writes──▶ LOCAL state slots  ──datagrams──▶
                       ◀──reads─── REMOTE state slots ◀──datagrams──
 event hooks ──push──▶ outgoing event queue           ──reliable stream──▶
 frame start ◀──drain── incoming command queue        ◀──reliable stream──
 ImGui overlay (hudhook) ◀──────── status / connect commands
```

Two rules hold everything together:

1. **Only the game thread touches game memory.** The network thread never dereferences a game pointer.
   It only reads and writes the shared slots and queues.
2. **Networking never runs inside a hook.** Hooks copy data in or out and return.

Authority model:

| Thing | Owner | How the other side sees it |
|---|---|---|
| Each player's own character | That player | Snapshots, smoothed between updates |
| Enemies, NPCs, world objects | Host | Host-issued spawns plus snapshots; the client's local spawns are deleted |
| Damage to enemies | Host | The client sends a hit *request*; the host applies it and broadcasts the result |
| Quests, cutscenes, area, save | Host | Reliable events; the client follows |

Do not attempt lockstep (both games simulating identically from the same inputs). The engine is not
deterministic.

## Folder structure (target)

- `src/lib.rs`: `DllMain` (trivial) and the worker thread that starts everything else.
- `src/proxy.rs`: the `dinput8.dll` proxy exports that forward to the real system DLL.
- `src/scan.rs`: pattern scanning, string and reference scanning, RIP-relative resolution.
- `src/addr.rs`: the address registry; every address is resolved once at startup.
- `src/sdk/`: reversed engine classes (`Pl0000`, `EmBase`, `cVec4`, `Behavior`, ...).
- `src/hooks/`: detours and mid-function hooks, one file per game system.
- `src/state.rs`: seqlock slots and queues shared between the game thread and the net thread.
- `src/net/`: the iroh endpoint, the protocol, and session state.
- `src/sync/`: game-side sync logic (player, entities, world, quests), called from hooks.
- `src/ui/`: the ImGui overlay.
- `notes/`: reversing notes, Ghidra/IDA exports, ReClass dumps.

## Strict rules

1. Every reversed struct is `#[repr(C)]`, with a byte-offset comment on every field, explicit padding for
   unknown gaps, and a compile-time size assert:
   ```rust
   #[repr(C)]
   pub struct cVec4 {
       pub x: f32, // 0x00
       pub y: f32, // 0x04
       pub z: f32, // 0x08
       pub w: f32, // 0x0C
   }
   const _: () = assert!(core::mem::size_of::<cVec4>() == 0x10);
   ```
2. No raw pointer arithmetic or dereference without a null check and a bounds check against the known struct
   size or the module range. Prefer `ptr.as_ref()` / `Option<&T>` over raw `*ptr`.
3. Keep `DllMain` trivial. Everything runs on the worker thread it spawns, because the loader lock is held
   inside `DllMain`.
4. Every packet from the network is untrusted. Decode with exact length checks, reject non-finite or absurd
   floats, clamp indices and IDs, and never turn a network value into a pointer.
5. Never overwrite files in `notes/`. Add new files only.
6. Each address pattern gets an `#[ignore]` test that runs it against the real exe (`NIER_EXE=<path>`), with
   the game build it was verified against written in a comment.
7. Code ported from AutomataMP carries a comment naming the source file, and the MIT notice stays in
   `LICENSE-THIRD-PARTY`.

## Build & test

```
cargo build --release
cargo test
NIER_EXE="C:\Program Files (x86)\Steam\steamapps\common\NieRAutomata\NieRAutomata.exe" cargo test -- --ignored
tools\install.ps1            # build + copy into the game folder as dinput8.dll (-Uninstall to remove)
```

- **Stable** Rust is pinned in `rust-toolchain.toml`. Nightly was only needed for retour's `static-detour`;
  the code uses retour's `GenericDetour` and `ilhook` instead, and naked functions are stable.
- If `link.exe` fails with `LNK1104`, the path is over 260 characters: set `CARGO_TARGET_DIR=C:\t`.
- Output: `target/release/nier_online.dll` (the host target is `x86_64-pc-windows-msvc`), deployed as
  `dinput8.dll` in the game folder.
- **Mirror test (one PC):** the overlay's **Mirror test** button makes the net layer echo your own state
  back to you, shifted 2 m sideways, so the remote-player code can be tested alone.
- **Two instances (one PC):** `tools/two-instances/launch-two.ps1` starts two copies of the game. It
  launches the exe directly with `SteamAppId=524220` (Steam must be running), then closes the game's
  single-instance lock, the named event `NieR:AutomataEvent`, inside the first copy before starting the
  second. Set both copies to windowed mode in the game's options. Both copies share one save folder and
  one mod config. That is fine: iroh binds random ports, and each copy picks Host or Join in its overlay,
  so no per-instance setting is needed. The overlay key only toggles the focused window.

## Libraries

| Purpose | Crate | Notes |
|---|---|---|
| Function hooks | `retour` (`GenericDetour`) | Typed detours for functions with a known signature |
| Mid-function hooks | `ilhook` | Rust equivalent of the safetyhook `MidHook` that AutomataMP uses; gives access to registers at any instruction. ilhook saves only xmm0-3, so `hooks/mod.rs` wraps every callback in a shim that also saves xmm4/5. It always writes 14 bytes; `scan::Image::check_hook_site` proves no branch lands inside them |
| Disassembly | `iced-x86` (decoder only) | Instruction-boundary walks for `scan_disasm`, RIP-relative targets, hook-site checks |
| Windows API | `windows` | |
| ImGui overlay | `hudhook` | DX11 renderer plus input capture, built on `imgui-rs` |
| Peer-to-peer networking | `iroh` | QUIC connections with hole punching and relay fallback; reliable streams and unreliable datagrams on one connection |
| Async runtime | `tokio` | Runs only on the net thread |
| Serialization | `serde` + `postcard` | Compact, versioned packets. AutomataMP used flatbuffers, which isn't needed when both sides are this same Rust code |
| Plain-data structs | `bytemuck` | Game-memory snapshots |
| Logging | `tracing` + `tracing-subscriber` + `tracing-appender` | Console and `nier_online.log` |

Use the latest stable version of each crate at the time it is added, and pin it in `Cargo.lock`.

---

# Roadmap

## Phase 0: Scaffold

> Status (2026-11-21): everything below through Phase 6 is implemented and unit-tested, and every address
> resolves against the build 7788 exe (`notes/2026-11-21-build-7788-addresses.md`). Nothing has run
> in-game yet, so only items that do not need the game are ticked. Next step: install, check
> `nier_online.log` for "all 16 addresses resolved" and the hook lines, then run the mirror test.

- [x] Cargo `cdylib` crate `nier_online`, edition 2024, stable pinned, target `x86_64-pc-windows-msvc`.
- [ ] `dinput8.dll` proxy: export `DirectInput8Create` and forward it to the real
      `%SystemRoot%\System32\dinput8.dll`. This is how the mod loads, with no injector needed.
- [ ] `DllMain` spawns one worker thread and returns. The worker sets up logging (a console in debug builds,
      plus a log file next to the exe) and then runs the later phases' setup.
- [x] `LICENSE` (GPL-3.0) and `LICENSE-THIRD-PARTY` (the AutomataMP MIT notice).

## Phase 1: Address resolution

Verified against the exe file on disk (the ignored tests), not yet observed at runtime.

- [x] Port `scan.rs` from nier_coop: pattern scan with wildcards, and RIP-relative resolution.
- [x] Add AutomataMP's more robust lookup techniques (see its `shared/utility` and `shared/sdk/EntityList.cpp`):
      - `scan_string`: find a unique string in the exe, such as `"MapInstance"` or `"core_AccelTime_Out"`.
      - `scan_reference`: find the instruction that references that string.
      - `scan_disasm`: from there, walk forward to the next `lea`/`mov` with a given opcode and read its
        target. Anchoring on strings survives game updates far better than raw byte patterns.
- [x] `addr.rs`: one struct holding every resolved address. It is built once at startup; if any lookup
      fails, log which one and install no hooks at all.
- [x] Start with the addresses nier_coop and AutomataMP already resolve: the entity list, the
      possessed-entity handle, the per-frame update, the entity spawn function and spawner object, the buddy
      routine, the script function table, and `start_animation`. (Also: held-buttons and pod hook sites,
      pod program, `Pl0000` vtable via RTTI, camera. Function starts come from `.pdata`, not `CC` padding.)
- [x] An `#[ignore]` test for every lookup, run against your installed exe.

## Phase 2: Game-thread hook and shared state

- [ ] Hook the per-frame update (nier_coop: a naked entry thunk that preserves the argument registers,
      because the signature is unknown). This is the game-thread tick where all sync logic runs.
- [x] Port nier_coop's `state.rs`: single-writer seqlock slots for "latest value" data such as snapshots.
- [x] Add bounded lock-free queues (for example `crossbeam-queue::ArrayQueue`) for discrete events in both
      directions. The game thread drains incoming commands once per frame.
- [ ] Port the `sdk/` structs used so far, following rule 1. Take field offsets from AutomataMP's
      `shared/sdk/*.hpp` and `reversing/nierautomata.genny`, and from nier_coop's `sdk/mod.rs`.

## Phase 3: ImGui overlay

AutomataMP draws an ImGui menu by hooking the DX11 swap chain (`src/hooks/D3D11Hook.cpp`) and the window
procedure for input (`WindowsMessageHook.cpp`). In Rust, `hudhook` does both.

- [ ] Overlay toggled with `Insert`. While it is open, block game input so typing doesn't move 2B.
- [ ] Connection panel: **Host** (shows the join code with a Copy button), **Join** (paste a code), and
      **Disconnect**. Show status, ping and the peer's mod version.
- [ ] Settings saved to `nier_online.toml` next to the exe: overlay key, player name, and debug flags.
- [ ] Debug panel, based on AutomataMP's `Explorer.cpp`: list live entities with class, handle, position
      and HP, and click one to inspect its fields. This is the main tool for reversing in later phases.
- [ ] Log panel showing the last few hundred `tracing` lines.

## Phase 4: Peer-to-peer networking

Implemented in `src/net/`. Notes: messages are split into `Datagram` and `Event` enums (one per channel).
The LAN shortcut is covered by the join code, which carries the host's direct addresses; a bare `IP:port`
join is not possible because iroh dials by public key. No automatic reconnect yet: after a drop, join again.
The bulk-transfer stream is not opened until Phase 8 needs it.

Goal: no port forwarding, no Tailscale, no dedicated server. `iroh` gives each player a key-based endpoint
ID. It connects directly through UDP hole punching when it can, and through a relay server when it can't
(for example when both players are behind strict carrier-grade NAT). The connection is encrypted QUIC.

- [ ] The net thread runs a single-threaded tokio runtime and owns the iroh `Endpoint`.
- [ ] Host: create the endpoint, accept one incoming connection, and show a **join code** (the iroh endpoint
      address/ticket, base32-encoded so it can be pasted).
- [ ] Client: parse the code and connect.
- [ ] Relays: start with iroh's default public relays. Check n0's current terms; if they don't suit a public
      release, self-host `iroh-relay` on a small VPS and put its URL in the config.
- [ ] Optional LAN shortcut: a direct `IP:port` join that skips the relay and discovery.
- [ ] Handshake on connect: a `Hello { protocol_version, mod_version, game_build }` exchange. Refuse
      mismatches with a clear message in the overlay.
- [ ] Channels on the one QUIC connection:
      - **Unreliable datagrams** for snapshots (players, enemies), 30–60 Hz, latest-wins, each with a
        sequence number so old ones are dropped.
      - **One reliable ordered stream** for events: spawns, deaths, damage, quest flags, area changes, chat.
      - **A separate reliable stream** for bulk transfers (the save file), so it doesn't block events.
- [x] `net/protocol.rs`: one `enum Message` with serde + postcard, and one decode function that enforces
      rule 4. Unit-test round trips and rejection of bad input.
- [ ] Timeout and reconnect: when the peer drops, restore single-player behavior (buddy AI back on, enemy
      spawning resumes on the client).
- [ ] Keep the design to two players (host + one client). Give every entity a network ID, so more players
      could be added later without changing the protocol shape.

## Phase 5: Remote player

Implemented in `src/sync/player.rs` on the delayed timeline in `src/sync/interp.rs`, which replaces nier_coop's
"move halfway to the newest packet" and its one-animation-per-packet limit. Not yet done: weapons and
loadout (writing `weapon_index` alone could crash), and the name tag's camera math is unverified.

Port this from nier_coop (`hooks/spawn.rs`, `hooks/mod.rs`). Its method came from AutomataMP's
`Player.cpp` and `PlayerHook.cpp`.

- [ ] Spawn a second 2B (`Pl0000`) and make her the local player's buddy, using the AutomataMP sequence
      (spawn, set as buddy, `changePlayer` twice).
- [ ] Suppress her buddy AI while connected. Drive her through her controller fields (stick, held and
      pressed buttons) plus replayed `start_animation` calls, never through the local gamepad.
- [ ] **Position correction:** inputs alone drift. Each snapshot carries position and rotation. Show her
      ~100 ms behind real time, smoothing between the two snapshots around that time; blend toward the
      true position when the error is small, and snap when it is large (for example over 3 m).
- [ ] Pod: fire through `ai_pod_fire`, aim at the peer's aim point (nier_coop's mid-function hook in
      pod_shot), and run pod programs. The engine only ticks the pod program and cooldown for the
      player-controlled character, so call the update for her and copy the peer's cooldown.
- [ ] Weapons and loadout, and the pod flashlight (both in AutomataMP's player sync list).
- [ ] Keep her invincible (`muteki_timer`) and show the peer's HP, because her death would fail the mission.
      Respawn her after the game removes her (area load, restart).
- [ ] Name tag above her head (ImGui, projected with the game camera; see AutomataMP `CameraGame.cpp`).
- [ ] Verify with the mirror test, then between two PCs.

## Phase 6: Enemy sync (host spawns, client mirrors)

First pass in `src/sync/entities.rs`, behind the `enemy_sync` setting (off by default). Gaps: client copies
still run their own AI, enemy animations are not replayed, `PlayerHit` is not implemented (client copies
hit the client locally instead), and damage is inferred from local HP loss rather than a damage hook.

This is AutomataMP's design (`src/mods/multiplayer/EntitySync.cpp`, the Go server's `SpawnEntity` and
`EntityData` handlers). It replaces nier_coop's approach of matching the client's own enemies to the
host's, which fails whenever the two games spawned different enemies.

- [ ] Hook the entity spawn function on both sides and read its `EntitySpawnParams` (model / object ID,
      name, position, rotation, set type; see AutomataMP `EntitySpawnParams` and `EntitySpawn.go`).
- [ ] **Host:** when an enemy or NPC spawns, give it a network ID and send `SpawnEntity { net_id, params }`.
- [ ] **Client:** let its own game spawn nothing that the network should own. When the game spawns a
      networked type on its own, delete it (AutomataMP: "deletes enemies on normal clients that are not
      supposed to be networked"). When `SpawnEntity` arrives, set a "host-requested" flag, call the spawn
      function with the host's params, clear the flag, and map `net_id` to the new handle.
- [ ] Turn off AI on the client's copies. Drive them with `EntityData` snapshots (position, rotation, HP,
      alive) at 20–30 Hz with the same smoothing as the remote player, plus `EntityAnimationStart` events.
- [ ] **Damage:** when the client hits a host-owned enemy, swallow the local damage and send
      `HitRequest { net_id, damage, attack type }`. The host applies it through the normal damage path and
      its next snapshot carries the new HP. Death and destruction are sent as reliable `DestroyEntity`
      events.
- [ ] Enemy attacks on the client's player: the client's copies don't attack on their own, so the host's
      hits on the remote 2B must be forwarded to the client (`PlayerHit { damage, knockback }`).
- [ ] **Late join / reconnect:** on connect, the host sends `SpawnEntity` for every live networked entity.
- [ ] Decide per class which entities are networked (enemies, bosses, destructibles, NPCs) and which stay
      local (effects, projectiles, cosmetic props). Keep the list in one table in `sync/entities.rs`.

## Phase 7: Areas and world state

- [ ] Detect area / map loads (AutomataMP finds `MapInstance` through its string).
- [ ] The host's area change is sent as a reliable event. The client follows: fast travel or warp to the
      same place, and show a "waiting for host" screen if the client's load finishes first.
- [ ] While loading, pause snapshots and queue events. Resume after both sides report "loaded".
- [ ] Respawn the remote player after every load (Phase 5).
- [ ] Sync world flags that change the map (opened doors, ladders, lifts, access points).

## Phase 8: Quests, cutscenes and saves

The public nier_coop repo doesn't include this. A newer nier_coop build (v0.0.8) has save, cutscene and
quest sync; its log shows it hooks `request_state`, the event signal post, the quest system's set/reset
functions, and the game's first save read. Port from that source if it is available; otherwise reverse it
again, using those hook points as a map.

- [ ] **Save sync at startup:** the client holds the game's first save read until the host's save arrives,
      writes it to a separate slot, and loads it, so both games start from identical progress. Never
      overwrite the client's own saves.
- [ ] **Quest flags:** hook quest set/reset on the host and replay them on the client. The client never
      sets quest flags from its own gameplay.
- [ ] **Cutscenes and scripted events:** relay phase / subphase signals and objective updates from the host.
      Both players watch the cutscene; neither can play until both finish (or the host skips).
- [ ] **Items and money:** decide shared vs. per-player loot. Start with "each player keeps their own pickups".
- [ ] Death and game over: if the host dies, both reload the host's checkpoint. If the client dies, respawn
      them next to the host instead of failing the mission.

## Phase 9: Special gameplay modes

Each of these swaps the player object or the camera. Every one needs a save made inside that section to
reverse it live.

- [ ] **Flight units:** the local player isn't a `Pl0000` here, so nothing is sent. Reverse the flight-unit
      class, then add a flight-unit variant to the player snapshot and spawn path.
- [ ] **2D / side-scrolling camera sections:** check that position correction respects the locked axis.
- [ ] **9S hacking minigames:** start with "the other player waits" and show a status line; sync later, if ever.
- [ ] **Boss fights with scripted phases:** confirm phase changes come only from the host.
- [ ] **Playing as 9S or A2:** spawn the remote player as the right model for the route.

## Phase 10: Polish and release

- [ ] Text chat in the overlay (an open feature request on AutomataMP).
- [ ] Connection quality display: ping, packet loss, relay vs. direct.
- [ ] Crash handler that writes a minidump and the last log lines (AutomataMP `ExceptionHandler.cpp`).
- [ ] Compatibility check with common mods such as FAR (Fix Automata Resolution), which also hook DX11.
- [ ] GitHub Actions: build on every push, attach `dinput8.dll` to tagged releases.
- [ ] README install guide with screenshots.
- [ ] After each game update, run the ignored address tests first, and fix patterns before anything else.

---

## Reference: what to take from each project

| Topic | nier_coop (port code) | AutomataMP (port design) |
|---|---|---|
| Pattern scanning | `src/scan.rs` | `shared/utility`: string, reference and disassembly scans |
| Engine structs | `src/sdk/mod.rs` | `shared/sdk/*.hpp`, `reversing/nierautomata.genny` |
| Thread handoff | `src/state.rs` seqlock | — |
| Remote player spawn | `src/hooks/spawn.rs` | `src/mods/multiplayer/Player.cpp`, `PlayerHook.cpp` |
| Pod sync | `src/hooks/mod.rs` | `MidHooks.cpp` |
| Enemy sync | — | `EntitySync.cpp`, server `handlers/SpawnEntity.go`, `EntityData.go` |
| Overlay UI | — | `D3D11Hook.cpp`, `WindowsMessageHook.cpp`, `Explorer.cpp` |
| Packet design | `src/net/mod.rs` (validation) | `schema/*.fbs` (message types) |
| Scripting VM | — | `ScriptFunctions.cpp` (mruby function table) |

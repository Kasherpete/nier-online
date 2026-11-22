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
| Damage to enemies | Host | The client's puppet on the host replays the client's attacks, so they damage the host's enemies through the engine's own damage path; snapshots carry the HP back |
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
- `src/sync/`: game-side sync logic, called from hooks and the per-frame tick:
  - `player.rs` (remote 2B, follow-host mode, re-link after cutscenes), `interp.rs` (delayed timeline),
  - `entities.rs` (enemy sync, pause detection), `world.rs` (entity list, classes, script natives),
  - `scene.rs` (cutscene + camera-event starts, `request_state` hook), `events.rs` (signals, SendCommand,
    objectives, enemy sets, scripted-scene steps, area triggers), `quest.rs` (story / quest flags).
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
  so no per-instance setting is needed. The overlay key only toggles the focused window. What makes two
  copies usable (all in `src/proxy.rs` and `src/ui/cursor.rs`, verified 2026-11-22):
  - Only the focused copy reads input: the exe's imports of `XInputGetState` (XINPUT1_4 ordinal 2) and
    `GetAsyncKeyState` are IAT-patched, and DirectInput reads return "nothing pressed" while unfocused.
  - The unfocused copy keeps running instead of opening its pause menu: `GetForegroundWindow` is IAT-patched to
    return the game's own window, and `WM_ACTIVATEAPP` / `WM_ACTIVATE(WA_INACTIVE)` / `WM_KILLFOCUS` are swallowed.
  - The unfocused copy leaves the mouse alone (`SetCursorPos` / `ClipCursor` dropped, clip released on focus loss).
  - Joining a host on the same PC dials it over `127.0.0.1` (Windows rejects the host's replies from its own LAN
    address with WSAEINVAL 10022). The host's identity is random per launch: copy a fresh join code after every
    relaunch (the log prints `hosting as <id>` / `joining <id>` to compare).
  - The client's overlay has a **Follow host** checkbox (connected, client only): its 2B copies the host's inputs
    and animations and stays 2 m beside the host, to test world sync alone.
- **Debug logging:** `log_level = "debug"` in `nier_online.toml` (next to the exe) also turns on iroh's and noq's
  debug output (path selection, hole punching, send errors). Set it back to `"info"` afterwards: logs get large.
  With a session up, every non-noisy scene-action state change is logged as `scene: action <id> <class> ...`
  (5000 lines per session); see `notes/2026-11-22-two-instance-and-world-sync-session.md`.

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

> Status (2026-11-22, v0.3.0, protocol 5): verified in-game: the mod loads, join codes connect, the overlay
> works, player sync and the name tag work, two copies run side by side on one PC, enemies appear on the client
> and stay in sync, and most of nier_coop's world sync is ported and working (story flags, signals, SendCommand,
> objectives, enemy sets; see Phase 8). Every address resolves against the build 7788 exe; the log should say
> "all 38 addresses resolved". Open problems, in order: (1) a camera event (scripted "mini cutscene") started on
> the client is accepted but does not take the camera (Phase 8); (2) a mini boss's attacks play on the host only
> (Phase 6/9); (3) the public n0 relay throttles the connection. Session notes:
> `notes/2026-11-22-two-instance-and-world-sync-session.md`.

- [x] Cargo `cdylib` crate `nier_online`, edition 2024, stable pinned, target `x86_64-pc-windows-msvc`.
- [x] `dinput8.dll` proxy: export `DirectInput8Create` and forward it to the real
      `%SystemRoot%\System32\dinput8.dll`. This is how the mod loads, with no injector needed.
- [x] `DllMain` spawns one worker thread and returns. The worker sets up logging (a console in debug builds,
      plus a log file next to the exe) and then runs the later phases' setup.
- [x] `LICENSE` (GPL-3.0) and `LICENSE-THIRD-PARTY` (the AutomataMP MIT notice).

## Phase 1: Address resolution

Verified against the exe file on disk (the ignored tests) and at runtime (2026-11-21 log: all addresses resolved,
every hook installed, player sync working).

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

- [x] Hook the per-frame update (nier_coop: a naked entry thunk that preserves the argument registers,
      because the signature is unknown). This is the game-thread tick where all sync logic runs.
- [x] Port nier_coop's `state.rs`: single-writer seqlock slots for "latest value" data such as snapshots.
- [x] Add bounded lock-free queues (for example `crossbeam-queue::ArrayQueue`) for discrete events in both
      directions. The game thread drains incoming commands once per frame.
- [x] Port the `sdk/` structs used so far, following rule 1. Take field offsets from AutomataMP's
      `shared/sdk/*.hpp` and `reversing/nierautomata.genny`, and from nier_coop's `sdk/mod.rs`.

## Phase 3: ImGui overlay

AutomataMP draws an ImGui menu by hooking the DX11 swap chain (`src/hooks/D3D11Hook.cpp`) and the window
procedure for input (`WindowsMessageHook.cpp`). In Rust, `hudhook` does both.

- [x] Overlay toggled with `Insert`. While it is open, block game input so typing doesn't move 2B.
- [x] Connection panel: **Host** (shows the join code with a Copy button), **Join** (paste a code), and
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

In-game findings (2026-11-22):
- A rejected event ends the session (`decode_event` error -> session over). Senders must therefore never send what
  the receiver rejects: player animations in `ANIM_CRASHES` or >= `MAX_ANIM` are not sent, and enemy spawn
  transforms are sanitised (a "non-finite spawn transform" ended a session).
- Same-PC joins dial loopback (see Build & test). Host / join / failures are logged (`hosting as`, `joining`,
  `network: ...`).
- The public n0 relay rate-limits ("outbound relay traffic is being throttled") during long sessions: another
  reason for the self-hosted relay below.
- Protocol history: 3 `CutscenePlay`, 4 `SceneEvent` / `SceneSignal` / `SceneStates`, 5 `SCENE_MIRROR`.

Goal: no port forwarding, no Tailscale, no dedicated server. `iroh` gives each player a key-based endpoint
ID. It connects directly through UDP hole punching when it can, and through a relay server when it can't
(for example when both players are behind strict carrier-grade NAT). The connection is encrypted QUIC.

- [x] The net thread runs a single-threaded tokio runtime and owns the iroh `Endpoint`.
- [x] Host: create the endpoint, accept one incoming connection, and show a **join code** (the iroh endpoint
      address/ticket, base32-encoded so it can be pasted).
- [x] Client: parse the code and connect.
- [ ] Relays: start with iroh's default public relays. Check n0's current terms; if they don't suit a public
      release, self-host `iroh-relay` on a small VPS and put its URL in the config.
- [ ] Optional LAN shortcut: a direct `IP:port` join that skips the relay and discovery.
- [x] Handshake on connect: a `Hello { protocol_version, mod_version, game_build }` exchange. Refuse
      mismatches with a clear message in the overlay. (Matching versions verified; the refusal path is not.)
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
loadout (writing `weapon_index` alone could crash). The puppet's weapon also sets how hard the client hits
enemies, since the puppet deals the client's damage on the host (Phase 6).

Port this from nier_coop (`hooks/spawn.rs`, `hooks/mod.rs`). Its method came from AutomataMP's
`Player.cpp` and `PlayerHook.cpp`.

- [x] Spawn a second 2B (`Pl0000`) and make her the local player's buddy, using the AutomataMP sequence
      (spawn, set as buddy, `changePlayer` twice).
- [x] Suppress her buddy AI while connected. Drive her through her controller fields (stick, held and
      pressed buttons) plus replayed `start_animation` calls, never through the local gamepad.
- [x] **Position correction:** inputs alone drift. Each snapshot carries position and rotation. Show her
      ~100 ms behind real time, smoothing between the two snapshots around that time; blend toward the
      true position when the error is small, and snap when it is large (for example over 3 m).
- [ ] Pod: fire through `ai_pod_fire`, aim at the peer's aim point (nier_coop's mid-function hook in
      pod_shot), and run pod programs. The engine only ticks the pod program and cooldown for the
      player-controlled character, so call the update for her and copy the peer's cooldown.
- [ ] Weapons and loadout, and the pod flashlight (both in AutomataMP's player sync list).
- [ ] Keep her invincible (`muteki_timer`) and show the peer's HP, because her death would fail the mission.
      Respawn her after the game removes her (area load, restart).
- [x] Name tag above her head (ImGui, projected with the game camera; see AutomataMP `CameraGame.cpp`).
- [ ] Verify with the mirror test, then between two PCs. (Mirror test and two copies on one PC verified.)
- [x] **Follow host** (client overlay checkbox, `state::FOLLOW_HOST`): the client's own 2B is driven by the same
      `drive` as the puppet, 2 m beside the host; the host's events and controller values are forwarded so the
      host's puppet of the client animates too. Only the puppet is made invincible. Position verified (stays
      within ~1 m); the forwarded animations on the host's screen are not verified yet.
- [ ] **Re-link after cutscenes** (nier_coop `spawn::relink`): when a cutscene ends (`scene::EVENT_ENDED`) or the
      buddy link breaks, she is unsuspended, given the player routine again and re-linked as buddy (at most once a
      second). Fires once after each spawn in-game; the post-cutscene case is not verified yet.

## Phase 6: Enemy sync (host owns enemies, client mirrors them)

Implemented in `src/sync/entities.rs` (second pass, 2026-11-22), behind the `enemy_sync` setting (on by default,
the host's setting decides). Reversing behind it: `notes/2026-11-22-enemy-sync-reversing.md`.

In-game findings (2026-11-22, two instances):
- The spawn hook did not report the enemies seen, so the host also scans the entity list every 250 ms.
- Many enemies have an empty entity name, so the host sends `em` plus the model's low 16 bits.
- The host sends only yaw (rotation .y) and a heartbeat batch every 50 ms even when it has no enemies.
  Before that change the client kept losing enemy updates after a few seconds, and its enemies went back to
  their own AI.
- Later the same day (details in `notes/2026-11-22-two-instance-and-world-sync-session.md`):
  - **Host pause:** the update hook keeps running in the pause menu, so frozen positions kept arriving and the
    client's enemies froze or ran in place. `detect_pause`: with enemies around, no bit of any sent enemy state or
    the host player's position changed for 300 ms, and no animation sent, means paused; the host sends nothing and
    the client's AI takes over after 1 s (logged both ways). It also fires during cutscenes and when everything
    stands still, which is harmless.
  - **Pairing** is closest-pair-first across a model (`assign`); greedy per-entity pairing matched waves 26 m apart.
  - **Copies** we spawned are never counted as "our own" entities (a lingering replaced copy was paired 37 m away).
  - An entity that survives 3 terminations is left alone (one em0050 survived termination every 2 s).
  - One log line per entity: `sending ... as <id>` (host), `paired with our own` / `spawned a copy` (client), and
    every 10 s `animations sent/played in the last 10 s` per enemy, to compare both sides.

The design combines AutomataMP's (`src/mods/multiplayer/EntitySync.cpp`, the Go server's `SpawnEntity` and
`EntityData` handlers: the host announces spawns and streams state, the client spawns copies) with the newer
local nier_coop's (`../nier_coop/src/hooks/world.rs` and `enemy_anim.rs`: pair the client's own enemies with
the host's, and block the client's enemy AI from starting animations). Pairing alone fails when the two games
spawn different enemies, and delete-and-respawn alone breaks the client's scripts, which wait on their own
enemies (quest triggers, boss phases, boss parts). So the client does both: it pairs where it can and
spawns or deletes where it can't.

- [ ] Hook the entity spawn function on both sides and read its `EntitySpawnParams` (model / object ID,
      name, position, rotation; see AutomataMP `EntitySpawnParams` and `EntitySpawn.go`). Hooked, but it
      missed the enemies seen in-game; the 250 ms list scan covers them. Find the spawn path enemy sets use.
- [x] **Host:** when an enemy or animal spawns, give it a network ID and send `SpawnEntity { net_id, params }`.
      Snapshots (position, yaw, HP) at 20 Hz in `Datagram::Entities`.
- [x] **Client, pairing:** back each host entity with one of the client's own entities of the same model within
      30 m, or the only one of that model on both sides at any distance (bosses). When nothing matches
      within 2 s, spawn a copy (the "host-requested" flag lets the spawn hook pass it), and swap the copy for
      the client's own entity if one turns up later. The client's own networkable entities that match nothing
      for 3 s are deleted (AutomataMP: "deletes enemies on normal clients that are not supposed to be
      networked").
- [x] **AI off on the client:** one detour on `EmBase::start_animation` (0x637970 on build 7788, shared by all 34
      enemy classes). The host sends every enemy's calls as `EntityAnim`; the client drops its own AI's calls
      on backing enemies and replays the host's. With `enemy_replay_unverified` (on by default since
      2026-11-22, no crash seen) every host call is replayed. Off, only (class, animation) pairs the client's
      own game has started are replayed (the allowlist learns from the blocked calls too).
- [ ] **Deaths:** backing enemies get `EmBase.onNoDieSuperArmor` (HP clamps at 1, no local stagger). The host
      sends `EntityDied` when the die state (0xAC / 0xA9) starts, or when HP has stayed at 0 for 1 s; the client
      clears no-die and calls `EmBase.setDieRtn`, the scripts' own kill, so its quest scripts see a real death.
      `DestroyEntity` follows when the host's entity leaves the list.
- [ ] **Damage:** no hit requests. The client's puppet on the host replays the client's attacks, so they hit the
      host's enemies through the engine's own damage path. The first pass also sent HP-loss-based
      `HitRequest`s, which counted every hit twice (and counted the host puppet's hits on the client too).
      The client shows its own hits at once and holds them up to 1.5 s until the host's HP reflects them.
- [ ] **Enemy attacks on the client's player:** expected to come from the replayed attack animations playing on
      the client's backing enemies (aimed at where the client stands, since the host's enemy was attacking the
      puppet there). If attack hitboxes turn out not to follow the animation, add `PlayerHit { damage,
      knockback }` from the host's hits on the puppet.
- [ ] **Late join / reconnect:** on connect, the host sends `SpawnEntity` for every live networked entity.
- [ ] Networked classes: `EmBase` (enemies and bosses) and `BaAnimal`; everything else stays local (boss parts,
      NPCs, hacking enemies, effects, bullets). The check is `ClassInfo::is_networkable` in `sync/world.rs`.
- [x] Session end, or no host snapshot for 1 s (host loading or paused): the client's AI runs its enemies again.
      Verified 2026-11-22 (host pause, cutscenes, area loads).
- [ ] **Bosses / mini bosses:** a mini boss (em0042, em5100) paired with the client's own but attacked only on the
      host. Next: compare its `animations sent` (host) with `animations played` (client). If the host sends almost
      none, its attacks do not go through `EmBase::start_animation` and the path it uses must be found.

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

That newer source is checked out next to this repo, at `../nier_coop` (GPL-3.0, port directly). The parts that
matter here are `src/hooks/events.rs` (cutscene play, SendCommand, objective state and enemy-set spawns,
retried until the client has loaded the action), `src/hooks/quest.rs` (scene-state sets and resets),
`src/hooks/savefile.rs` with `src/net/save.rs` (holding the first save read, save transfer), and
`notes/quest-state.md` and `notes/death-and-warp.md`. Its offsets are for the 2026-10-09 exe; nine of them
matched build 7788, but re-resolve every one. (On this machine that source is at `C:\Users\micah\Nier Coop Mod`.)

### What is ported (2026-11-22, v0.3.0)

Everything resolves statically in `addr.rs` (all values equal nier_coop's for the 2026-11-19 exe, pinned by
`resolves_against_exe`): `request_state` 0x3ec650 (hooked), the action scene 0xfca888, the state-request builder
0x3cffb0 (`PhaseEventAction.load` + 0x77), the signal post 0x34cab0 (hooked) and queue 0xfca918 (`Action.signal`
+0x53 / +0x48), the SceneStateSystem 0xfc2680 with its `set` / `reset` natives and string pool calls (hash 0x28ee30,
intern 0x150e70, release 0x150c60), and the class vtables PhaseEventAction 0xc40758, CameraAction 0xc40b60,
SendCommand 0xc46d70, HapGoalLocationAction 0xc4ce60, EnemySetArea 0xc40458, EnemySetAction 0xc403c0,
PlayerCombatLimitAction 0xc41b08, AreaAction 0xc4c538. Scene-action state hashes are tabled in the session note.

- `sync/scene.rs`, **cutscene starts, both ways** (nier_coop: host -> client only). When a `PhaseEventAction`
  reaches PLAYING, or a `CameraAction` reaches PLAY (681c330e), its layout id goes out as `CutscenePlay`; the
  other game plays its own action with that id (`PhaseEventAction.play`, or `request_state(PLAY)` for a camera
  event), one at a time, waiting up to 60 s for it to load. A later start of the same action object is refused
  for the session (a cutscene only after it reached DONE, so its own mid-play requests pass); a reload (our
  character's handle changes) clears that. Simultaneous triggers on both games are deduplicated.
- `sync/events.rs`, **world events**, ported from nier_coop `events.rs`: host scene signals are posted on the client
  from its own action with the same id (sender contexts are learned from the client's own posts; a cutscene's own
  signals are not forwarded); kind 1 SendCommand runs; kind 2 objectives (host -> client, and the client finishing
  one first finishes it on the host); kind 3 EnemySetArea spawns; kind 4 `SCENE_MIRROR`, new here: EnemySetAction,
  PlayerCombatLimitAction and AreaAction state changes copied as they are. Events wait up to 180 s for their action
  to load on the client.
- `sync/quest.rs`, **story / quest flags** (nier_coop `quest.rs`): the host's SceneState names go out as
  `SceneStates` when they change (checked every second, resent every 3 s); the client sets what it lacks and
  resets what the host lacks after 3 lists in a row. `Buddy/`, `PL/`, `DLC_`, `playgo`, `auto/` stay local.

- [ ] **Save sync at startup:** the client holds the game's first save read until the host's save arrives,
      writes it to a separate slot, and loads it, so both games start from identical progress. Never
      overwrite the client's own saves. (Not ported yet: nier_coop `savefile.rs` / `net/save.rs`.)
- [x] **Quest flags:** the host's story states are copied to the client (`sync/quest.rs`). Verified 2026-11-22: on
      every join the client set the 33 states its older save lacked (`quest sync: + ...`).
- [ ] **Cutscenes and scripted events:** relay phase / subphase signals and objective updates from the host.
      Both players watch the cutscene; neither can play until both finish (or the host skips).
      Verified in-game: PhaseEventAction cutscenes start on both, signals, SendCommand runs, objectives and
      enemy-set spawns follow the host within ~10-30 ms, and a whole scripted scene's steps run on the client in the
      host's order. **Open:** a camera event the client is told to play is accepted and stays in PLAY, but the
      client's camera stays on its 2B (with the scene steps mirrored, it flickers once); the subtitles play. The
      host's scene starts from an area trigger the client's player is not in, so v0.3.0 mirrors AreaAction too
      (untested). Not done: waiting for both players before a cutscene ends, and the host skipping.
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
- [ ] **Boss fights with scripted phases:** confirm phase changes come only from the host. With Phase 6
      pairing, the client's own boss backs the host's (the only one of its model, so it pairs at any
      distance). The boss keeps its parts and the client's phase scripts, its phase animations are replayed
      from the host, and its HP follows the host's. Check this per boss; bosses whose parts carry their own
      HP may need the parts synced too.
- [ ] **Playing as 9S or A2:** spawn the remote player as the right model for the route.

## Phase 10: Polish and release

- [ ] Text chat in the overlay (an open feature request on AutomataMP).
- [ ] Connection quality display: ping, packet loss, relay vs. direct.
- [ ] Crash handler that writes a minidump and the last log lines (AutomataMP `ExceptionHandler.cpp`).
- [ ] Compatibility check with common mods such as FAR (Fix Automata Resolution), which also hook DX11.
- [ ] GitHub Actions: build on every push, attach `dinput8.dll` to tagged releases. Written: every push to
      `main` replaces the `latest` pre-release (`nier-online.zip` with `dinput8.dll` and `install.ps1`), and
      `v*` tags publish permanent releases. Not yet run on GitHub.
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
| Enemy sync | `../nier_coop`: `src/hooks/world.rs` (pairing), `enemy_anim.rs` (AI blocking) | `EntitySync.cpp`, server `handlers/SpawnEntity.go`, `EntityData.go` |
| Overlay UI | — | `D3D11Hook.cpp`, `WindowsMessageHook.cpp`, `Explorer.cpp` |
| Packet design | `src/net/mod.rs` (validation) | `schema/*.fbs` (message types) |
| Scripting VM | — | `ScriptFunctions.cpp` (mruby function table) |
| Cutscenes, signals, world events | `src/hooks/events.rs` (ported to `sync/scene.rs` + `sync/events.rs`) | — |
| Quest / story flags | `src/hooks/quest.rs` (ported to `sync/quest.rs`) | — |
| Save sync | `src/hooks/savefile.rs`, `src/net/save.rs` (not ported yet) | — |
| Two games on one PC | `src/hooks/focus.rs` (background run, input) | — |

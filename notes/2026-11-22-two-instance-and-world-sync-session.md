# Two-instance testing and world sync (2026-11-22, build 7788)

Findings from a long live session with two copies of the game on one PC (`tools/two-instances/launch-two.ps1`),
host in one window and client in the other, read from `nier_online.log` / `nier_online.2.log`. Build 7788
(`Last_SVN_REVISION=7788`, PE timestamp `0x6ac89519`). RVAs are exe-relative.

## Two copies on one PC

- **Inputs reached both copies.** XInput (`XINPUT1_4` ordinal 2, imported by ordinal) and `GetAsyncKeyState` read
  the hardware regardless of focus. `proxy::gate_unfocused_input` repoints the exe's IAT entries to wrappers that
  return an idle pad / nothing pressed while the window is not in front; DirectInput reads are held the same way.
- **Alt-tab paused the other copy.** The game checks `GetForegroundWindow` (IAT-patched to always return its own
  window once seen) *and* reacts to `WM_ACTIVATEAPP` / `WM_ACTIVATE(WA_INACTIVE)` / `WM_KILLFOCUS` (swallowed by a
  window-procedure wrapper, `proxy::keep_running_unfocused`). Activation messages still pass.
- **Mouse stuck to one window.** Since the game never learns it lost focus, it keeps calling `SetCursorPos` /
  `ClipCursor` every frame. The existing `ui::cursor` detours now drop those while unfocused, and the clip is
  released when focus is lost.
- **Joins timed out (loopback fix).** iroh debug logs showed the host receiving the client's first packet on the
  LAN address and every reply failing: `noq_udp sendmsg error 10022 (WSAEINVAL)` when sending from the PC's own LAN
  IP to itself with an explicit source address. `net::loopback_if_same_pc` replaces the host addresses that belong
  to this PC with `127.0.0.1:<port>`; joins now connect in ~10 ms. Unrelated to real two-PC play.
- **Stale join codes.** The iroh secret key is random per game process, so a join code only works until the host
  copy restarts. Host and join now log `hosting as <id> at {...}` / `joining <id> at {...}` to spot that.
- **Relay throttling.** The public n0 relay (`use1-1.relay.n0.iroh.link`) rate-limited the endpoint repeatedly
  ("outbound relay traffic is being throttled"). On one PC traffic should be direct (loopback); over the internet
  this needs a self-hosted relay (Phase 4).
- **Different save slots.** Both copies share one save folder, but the client copy kept loading a slot 33 story
  states behind the host's; quest sync fixes it each load, but loading the same slot in both is cleaner.

## Pause and enemies

- The per-frame update hook keeps running while the host is in the pause menu, so it kept sending frozen enemy
  positions and the client's enemies froze or ran in place. `entities::detect_pause`: with enemies around, a frame
  hash (every sent enemy's position/yaw/HP bits + the host player's position) unchanged for 300 ms and no animation
  sent means paused; the host then sends nothing and the client's AI takes over after `STALE_MS` (1 s). It also
  fires during cutscenes and when everything stands still.
- Pairing was greedy per host entity in HashMap order: waves paired up to 26 m apart. Now `entities::assign` takes
  the closest (host, ours) pairs first across a model.
- A replaced copy lingers after termination and was paired with another host entity 37 m away, then re-paired in a
  chain. Copies we spawned are now never "our own" (`copies` set).
- One em0050 survived termination every 2 s for minutes: an entity is left alone after 3 failed terminations.
- Spawn announcements sent raw rotation/position; one non-finite value made the client reject the event and end the
  session ("non-finite spawn transform"). Now sanitised like snapshots (position w = 1, yaw only).
- A mini boss (seen as em0042 and em5100) paired fine but its attacks did not show on the client. Unresolved; the
  10 s `animations sent/played` log lines count `EntityAnim` per enemy on both sides to see if its attacks go through
  `EmBase::start_animation` at all.

## Scene actions (cutscene / world sync)

Every scene action changes state through `Action::request_state` (0x3ec650). State hashes seen:

| Class (vtable) | States |
|---|---|
| PhaseEventAction (0xc40758) | 6825c6ae, bb7f80a1 (load), dfc1471f, a09a3421 PLAY_REQ, ecbccdf8 PLAYING, b649891d (mid-play, client only), cee3b8c9 PLAY_END, 26b80d1f DONE |
| CameraAction (0xc40b60) | 0 -> 4b7b6e51 WAIT on area load; 0 -> 681c330e PLAY when the scene starts it |
| CameraTargetAction (0xc40a98) | bbe63b84 -> e5254d85 |
| PlayerCombatLimitAction (0xc41b08) | 0 -> 4b7b6e51 -> bbe63b84 (lock), back to 4b7b6e51 when the camera move ends |
| EnemySetAction (0xc403c0) | 0 -> e361d0b2 |
| EnemySetArea (0xc40458) | 0 -> 4b7b6e51 WAIT -> 3aa89d11 SPAWN -> e361d0b2 |
| DelayAction (0xc4cc08) | 0 -> 45ce422b (counting; its signal 45ce422b carries the remaining count) -> 26b80d1f |
| SendCommand (0xc46d70) | 0 -> 26b80d1f; on the client some stop in 1edefddb |
| HapGoalLocationAction (0xc4ce60) | 0 -> 4b7b6e51 WAIT -> bbe63b84 ACTIVE -> 26b80d1f DONE |
| AreaAction (0xc4c538), ChangeAreaAction (0xc43cf0), AreaCommand (0xc4c5d0) | 93497d6e, cc7564cb, 4a063532, 8aaf03bc |

The enemy-wave "mini cutscene" at the first camera event (CameraAction 6091fb73) is a chain, all in one frame on
the host: area trigger c758d055 (signals cc7564cb, 4a063532, 93497d6e, 8aaf03bc; class not logged, AreaAction
assumed) -> EnemySetAction 864ec3e4 -> PlayerCombatLimitAction 5882b43b WAIT, ACTIVE -> CameraAction 6091fb73 PLAY
-> DelayAction 832ccbf3 (3 s) -> SendCommands f58eb738, 9cb2bacb, 98094304 -> DelayAction 0842848b (5 s) ->
SendCommands c880a6bf, de4bcc32, combat limit released -> DelayAction d8aaa62f (3 s) -> SendCommands d7d5af04,
3f6cbb71, f2a8ba1b, 8c0daf53, objective 23b73f94 WAIT/ACTIVE, objective b0fcadca DONE.

Client results, step by step:
- Requesting PLAY on the client's CameraAction is accepted (`ok 1`) and the state stays 681c330e, but the camera
  stays on the client's 2B. Mirroring EnemySetAction + PlayerCombatLimitAction first made the screen flicker once.
  Not forwarding the camera action's own signals did not change it. Next try: mirror the AreaAction trigger, so the
  client's own scene runs the chain (build 0.3.0).
- PhaseEventAction cutscenes started on both games when both players reached the trigger; dedupe dropped the second.
- SendCommand, objective, enemy-set and quest-state sync from nier_coop all worked in-game (each step mirrored
  within ~10-30 ms, objectives retried until the client loaded them, 33 story states applied on join).

## Follow host

The client's own character is driven with the same `drive` as the remote puppet, 2 m beside the host. Writing
position worked (logged distance from the target stays under ~1 m). Two bugs found: the puppet's invincibility and
HP copy were applied to the local follower too (enemies dealt no damage), and the host saw no animations because
replays go through the original `start_animation` (bypassing the detour that sends them) and `capture_local` ran
before the controller fields were written. Both fixed.

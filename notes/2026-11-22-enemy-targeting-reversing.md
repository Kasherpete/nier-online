# Enemy targeting reversing (2026-11-22, build 7788)

Static analysis of the build 7788 exe (PE timestamp `0x6ac89519`) with iced-x86. RVAs are exe-relative.
Checked by the ignored test `player_getters_and_enemy_updates` in `src/addr.rs`. Used by `src/sync/aggro.rs`.

## The problem seen in-game

With the host away from an enemy, the enemy stayed passive on both games, even with the client standing
next to it, until the client hit it. The client's enemies mirror the host's (Phase 6), so the question is why
the host's enemy ignores the client's puppet.

## How enemy code finds "the player"

- The possessed handle (0x12505dc) is read in only ~120 places, all in 0x7c0000..0x845000: script natives
  and a handful of small getters. Enemy code never reads it directly; it calls the getters.
- Getters (all `possessed` -> `copy handle` 0x7450a0 -> `handle_to_entity` 0x745f20 -> ...):

  | RVA | What | Callers (in 0x590000..0x6b0000) |
  |---|---|---|
  | 0x7c4f50 | `get_player() -> Pl0000*` (0x3085c0 casts the entity to Pl0000, null otherwise) | 601 (182) |
  | 0x7c5020 | `get_player_entity() -> Entity*`: `lea rcx,[possessed]; jmp 0x745f20`, no `.pdata` | 85 (11) |
  | 0x7c5050 | `get_player_pos(out) -> out`: player+0x50, or (0,0,0,1) | 31 (3) |
  | 0x7c4610 | `get_buddy() -> Pl0000*`: the player's `+0x1647C` handle (the buddy) | 24 (6) |
  | 0x7c4500 | the buddy if its `+0x5B8` model is 0x10000 | 5 (1) |
  | 0x7c4880 | like 0x7c5050, other position | 30 (0) |
  | 0x7ca790 | money drop (`core_dropitem_get`) | 5 (2) |

- `EmBase` target: `0x63ed20(this, &handle)` stores the target handle at `EmBase+0x5EB4`. Callers pass
  `get_player_entity()` (e.g. Em1100's slot 5 at 0x655376 -> 0x65539f) or the buddy's handle (Em1100's slot 7
  0x5ff1f0 when `+0x594 == 3`). `EmBase.setHapAttackTarget` (native 0x62e740 -> 0x6313b0) is a separate,
  script-forced target at `+0x2AD48`.
- So, with no special case, an enemy's idea of "the player" is the possessed character only. The puppet is
  the host player's buddy, which enemy code looks at only in a few class-specific places.

## Enemy update slots

- `Behavior` vtable slot 5 is per-entity setup (EmBase 0x65f730: huge stack frame, returns success). Slot 7 and
  slot 8 are the per-frame updates (both count timers down by the entity's frame time, 0x48f9c0).
- EmBase slot 7 = 0x6009f0. It calls BehaviorAppBase's slot 7 (0x415100) first, then runs the AI through
  virtual calls, among them slot 202 (`[rax+650h]`, 0x58c560, 9 getter calls within 4 call levels). EmBase slot
  8 = 0x688dd0 (8 getter calls within 4 levels). Every enemy class overrides slot 7 (Em1100 0x5ff1f0,
  Em1000 0x5fee40, Em0000 keeps 0x6009f0), and those call 0x6009f0 directly.
- Slots 7 and 8 use only `rcx` (`this`) on entry in the classes checked (EmBase, Em0000, Em1000, Em1100).
- 34 classes derive from EmBase; all their primary vtables are in `.rdata`.

## What the mod does

- Every EmBase-derived vtable's slots 7 and 8 point at a wrapper that records, per thread, whether that
  enemy should target the puppet (nearer than the host's character by 3 m; it switches back when the host is
  3 m nearer). While it is set, the three getters 0x7c4f50 / 0x7c5020 / 0x7c5050 return the puppet.
- Host only, and only while the puppet exists and is live. The setting `enemy_target_client` turns it off.

## Open questions (need the game running)

- Is the passive state really decided by these getters, or (also) by an area trigger that tracks only the
  possessed character? If enemies still ignore the client, look at the enemy-set / area code next.
- Does any enemy code do something to "the player" that is wrong for the puppet (camera, lock-on UI, grabs)?
- Boss parts (Em*Parts, not EmBase) are not wrapped: do any bosses aim through their parts' updates?
- Does the engine run entity updates on more than one thread? The context is thread-local either way.

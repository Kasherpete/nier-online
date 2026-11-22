# Enemy sync reversing (2026-11-22, build 7788)

Static analysis of the build 7788 exe (`Last_SVN_REVISION=7788`, PE timestamp `0x6ac89519`) with iced-x86.
RVAs are exe-relative. Checked by the ignored tests `resolves_against_exe`, `enemy_classes_share_start_animation`
and `enemy_natives` in `src/addr.rs`.

## Enemy classes

- RTTI lists 2764 vtables. 34 classes derive from `EmBase` (`EmBase` vtable 0xc79d78): Em0000..Em9010, plus the
  bosses (Em1000, Em1100, Em2000, Em3000, Em4000, Em5000, Em7000, Em8000, Em8800, Em9000, ...).
- Boss parts are separate `BehaviorAppBase` classes, *not* `EmBase`: Em0110Parts, Em2000Parts, Em2100Parts,
  Em5000Arm / ArmBig / Block / Block2 / FloatFloor / FloatRubble / HealBlock, Em8800Parts, Em9000Parts,
  Em4000Weapon (3 classes). The boss spawns them itself, so a client whose own boss backs the host's keeps them.
- Also direct `BehaviorAppBase` children: BaAnimal (4 classes), NPC, HackingEnemy (10), Pl0000, Pl0013, Pl1010,
  BehaviorFlightUnit, BehaviorWeapon, BehaviorBullet, BehaviorPodLaser, Et2000, Et2020, EndRollEnemy.

## start_animation (vtable slot 18)

- `EmBase::start_animation` = 0x637970, `(this, anim, variant, a3, a4)` like `Pl0000`'s (0x433c70; 5th argument
  on the stack). It remaps a few ids for some models, calls vtable+0x560 with the current state (+0x670), then
  the BehaviorAppBase version 0x416bf0.
- Slot 18 of the 34 classes: 26 point at 0x637970, 7 (Em1000, Em1100, Em3000, Em4000, Em5000, Em9000, ...) at
  the thunk 0x6378d0 (`jmp 0x637970`), Em0040 at 0x6378e0 (calls 0x637970, then zeroes +0x2C3A0). So one detour
  on 0x637970 sees every enemy's animation calls.
- BaAnimal classes use 0x4269b0 (not hooked).

## HP, no-die, death

- `BehaviorAppBase.getHp` / `getHpMax` are vtable +0xF8 / +0xF0. `EmBase.setHpFromRate` (0x62f300) writes
  `+0x858 = +0x85C * rate`: HP is +0x858, max HP +0x85C.
- `EmBase.onNoDieSuperArmor` (0x5f1ba0) / `offNoDieSuperArmor` (0x5f0890): set / clear `EmBase+0x2C330`.
  Readers:
  - damage application 0x66d2f0 (`this, damage, hit_info, f32`) and 0x66d7ad: after subtracting, `if no_die &&
    hp < 1: hp = 1`.
  - hit reaction check 0x5842b0 and 0x58c560: skip the reaction (super armor).
- 0x66d2f0 never starts the death itself.
- `EmBase.setDieRtn` (0x537128) = `jmp [vtable+0x550]` (slot 170). EmBase's slot 170 = 0x614720: checks
  0x49eb50(this, 0xA9), then `start_animation(this, 0xAC, 0, 0, 0)`, or 0xA9 for model 0x2B000 and when obj_flags
  bit 0x2000 is clear. So the enemy die states are 0xAC and 0xA9 (`sdk::EM_DIE_STATES`).
- `EmBase.setDieRtnNoDrop` (0x6147a0) sets +0x28040 and +0x28044 to 1 first: the same death without drops.
- Other natives seen: `EmBase.setMutekiTimer` writes f32 +0x5F28; `EmBase.clearUndead` zeroes +0x2BCFC;
  `EmBase.setDisappearDie` sets +0x28178.

## Script native registration

Each script function has a static initializer: `lea rax,[native]`, `lea rcx,[name]`, `call 0x28ee00` (CRC),
then links the node into the list at 0xfc9888. So the native for a name is the `lea rax` target in the
function that references the name string.

## Open questions (need the game running)

- Do backing enemies whose own AI calls are blocked stay stable, or does their AI misbehave?
- Does a replayed attack animation activate the attack's hitbox on the client (hurting the client's player)?
- Does `setDieRtn` on a client boss fire its quest script exactly as a normal kill does?
- Does the puppet's damage on the host match what the client's own hits would deal (buddy damage scaling,
  weapon not synced yet)?

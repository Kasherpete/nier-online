# Save file I/O (build 7788, static analysis 2026-11-22)

Found with throwaway ignored tests over the exe on disk (string refs, callers, import calls).

- One path builder, `0x282cd0(kind, slot)`, formats `%sSystemData.dat`, `%sGameData.dat` and `%sSlotData_%d.dat`
  (the folder is `Documents\My Games\NieR_Automata\`). It is the only reference to each string.
- Its four callers are one dispatcher and three jobs. The dispatcher, `0x281000` (EnterCriticalSection, a job
  switch), runs `DeleteFileA` itself and calls the three jobs from 0x2811de / 0x2811e8 / 0x2811f2:
  - `0x282d70`: `CreateFileA(GENERIC_READ)` + `ReadFile`: plain read.
  - `0x282f60`: `CreateFileA(GENERIC_READ)` + `ReadFile` plus a `SteamInternal_ContextInit` call: the read that
    checks the slot's owner (a slot holding another account's SteamID64 at file offset 4 shows as empty; see
    nier_coop `net/save.rs set_owner`).
  - `0x2831e0`: `CreateFileA(GENERIC_WRITE)` + `WriteFile`, also with the Steam context: save.
- So every save read, write and delete is a job that opens the file by name through `CreateFileA`, on the
  save worker. Loading a slot from the menu is expected to open the file again (not verified in-game). The
  save list itself is only read once, after the startup logos (nier_coop: "a save written while the game runs
  shows after a restart").
- Every `SlotData_N.dat` is 235980 bytes. GameData.dat starts with the player's SteamID64 (high word
  `01 00 10 01`); each slot stores its owner's at offset 4.
- The save folder has `steam_autocloud.vdf`: Steam Cloud syncs it. Files the mod writes go in the game folder.

Used by `src/hooks/savefile.rs` (CreateFileA / DeleteFileA redirect) and `src/net/save.rs`.
Open: which of the two read jobs the title's save list uses, and whether "Continue" reads GameData.dat to pick
the slot.

# Build 7788 address resolution (2026-11-21)

Exe: `NieRAutomata.exe`, Steam `Last_SVN_REVISION=7788` (`Last_GIT_COMMIT=db802f67...`), 17,613,568 bytes,
file dated 2026-11-21, PE timestamp `0x6ac89519`, preferred base `0x140000000`, 53,673 `.pdata` functions.

Statically imports `DINPUT8.dll`, so the `dinput8.dll` proxy loads at process start. Other imports:
KERNEL32, USER32, SHELL32, d3d11, dxgi, steam_api64, WINMM, XINPUT1_4, SETUPAPI, GDI32, ole32, OLEAUT32.

Sections: `.text` 0x1000 (0xc1a000), `.rdata` 0xc1b000, `.data` 0xf04000, `.pdata` 0x17af000, `_RDATA`,
`.gehcont`, `.rsrc`, `.reloc`. No packer: patterns match the file on disk.

RVAs resolved by `addr::resolve` (from `cargo test resolves_against_exe -- --ignored`):

| Name | RVA | How |
|---|---|---|
| entity_list | 0x1029860 | AutomataMP pattern, `add rcx,[rip+x]` |
| possessed | 0x12505dc | `"core_AccelTime_Out"` ref -> `lea rdx` |
| update | 0x7b4d00 | pattern -> `.pdata` function start |
| spawn / spawner | 0x745af0 / 0x1029898 | `"MapInstance"` ref -> `lea rcx; call` |
| routine | 0x3ec540 | `"buddy_pascal"` ref -> `jmp` |
| scripts | 0xfc9888 | `"EntityLayout.enableCreateEntity"` ref -> `mov rax` |
| held_store | 0x52a7aa | button-processing function -> `mov [rcx+0x740],rax` |
| pod_no_target / aim copy | 0x48b943 / 0x48b992 | pod_shot pattern |
| pod_program | 0x529080 | pattern |
| Pl0000 vtable | 0xc61628 | RTTI `.?AVPl0000@@` -> locator -> vtable |
| start_animation | 0x433c70 | vtable slot 18 |
| camera | 0x1020bb0 | `"Step : %d / %d"` ref -> `lea rcx` |

The first nine match what nier_coop resolved at runtime on the same exe (its log shows module base
0x7ff661fc0000). All five patched sites pass `check_hook_site` (14-byte patch, no branch into it).

`Pl0000` class chain: Pl0000 : BehaviorAppBase : Behavior : cObj : cModel : cParts.

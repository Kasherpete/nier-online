//! Game-memory access helpers: entity list, possessed entity, RTTI class checks, script functions and the
//! camera. Game threads only. Every pointer is null-checked, and objects are only treated as a struct after
//! their vtable is confirmed to be in the module and of the right class (CLAUDE.md rule 2).
//!
//! Entity list and possessed-handle access port AutomataMP `shared/sdk/EntityList.cpp`; script function
//! lookup ports AutomataMP `shared/sdk/ScriptFunctions.cpp`; the RTTI check ports AutomataMP
//! `shared/utility/RTTI.cpp` (`derives_from`).

use std::collections::HashMap;
use std::ptr::{addr_of, addr_of_mut};
use std::sync::{Arc, LazyLock, Mutex};

use crate::hooks::game;
use crate::sdk::{self, BehaviorAppBase, CameraGame, Entity, EntityDescriptor, Pl0000, ScriptFunction, rtti};

/// Upper bound for the entity count, far above anything the game uses; a larger value means the list
/// global is not what we think it is.
const MAX_ENTITIES: usize = 0x10000;

fn list() -> Option<(*const EntityDescriptor, usize)> {
    let g = game();
    let at = g.addr.abs(g.addr.entity_list);
    // SAFETY: both globals were resolved inside the module's data section.
    let (list, count) = unsafe {
        (
            (at as *const *const EntityDescriptor).read_volatile(),
            ((at - sdk::ENTITY_COUNT_BEFORE_LIST) as *const u32).read_volatile() as usize,
        )
    };
    (!list.is_null() && count <= MAX_ENTITIES).then_some((list, count))
}

/// Live entity for `handle`; rejects out-of-range indices and stale handles like the engine does.
pub fn entity_by_handle(handle: u32) -> Option<*mut Entity> {
    if handle == 0 {
        return None;
    }
    let (list, count) = list()?;
    let i = ((handle >> 8) & 0xFFFF) as usize;
    if i >= count {
        return None;
    }
    // SAFETY: `i < count`, the engine's own bound for this array.
    let d = unsafe { list.add(i).read_volatile() };
    ((d.handle ^ handle) & 0xFFFF_FF00 == 0 && !d.ent.is_null()).then_some(d.ent)
}

/// Every live (handle, entity).
pub fn entities() -> Vec<(u32, *mut Entity)> {
    let Some((list, count)) = list() else { return Vec::new() };
    (0..count)
        .filter_map(|i| {
            // SAFETY: `i < count`.
            let d = unsafe { list.add(i).read_volatile() };
            (!d.ent.is_null() && d.handle != 0).then_some((d.handle, d.ent))
        })
        .collect()
}

pub fn possessed_handle() -> u32 {
    let g = game();
    // SAFETY: resolved global u32 in the module.
    unsafe { (g.addr.abs(g.addr.possessed) as *const u32).read_volatile() }
}

pub fn entity_handle(e: *mut Entity) -> u32 {
    // SAFETY: callers pass entities taken from the live list.
    unsafe { addr_of!((*e).handle).read() }
}

pub fn entity_name(e: *mut Entity) -> String {
    // SAFETY: as above; the name is a fixed 0x20-byte array.
    unsafe { (*e).name_str().to_string() }
}

// -------------------------------------------------------------------------------------------------------
// Classes

#[derive(Debug, Default)]
pub struct ClassInfo {
    /// Decorated names, most-derived first.
    pub chain: Vec<String>,
    pub is_app_base: bool,
    pub is_pl0000: bool,
    /// Enemies and animals: the host owns them (AutomataMP `Behavior::is_networkable`).
    pub is_networkable: bool,
}

impl ClassInfo {
    pub fn short_name(&self) -> String {
        self.chain.first().map_or("?".into(), |n| n.trim_start_matches(".?AV").trim_end_matches("@@").to_string())
    }
}

static CLASSES: LazyLock<Mutex<HashMap<usize, Arc<ClassInfo>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Class of the object whose vtable pointer is `vtable` (absolute). Cached per vtable.
pub fn class_of_vtable(vtable: usize) -> Option<Arc<ClassInfo>> {
    let g = game();
    if !g.in_module(vtable) {
        return None;
    }
    if let Some(c) = CLASSES.lock().ok()?.get(&vtable) {
        return Some(c.clone());
    }
    let chain = rtti::class_hierarchy(&g.img, vtable - g.addr.base).unwrap_or_default();
    let has = |n: &str| chain.iter().any(|c| c == n);
    let info = Arc::new(ClassInfo {
        is_app_base: has(".?AVBehaviorAppBase@@"),
        is_pl0000: vtable == g.addr.abs(g.addr.pl0000_vtable),
        is_networkable: has(".?AVEmBase@@") || has(".?AVBaAnimal@@"),
        chain,
    });
    CLASSES.lock().ok()?.insert(vtable, info.clone());
    Some(info)
}

/// The entity's behavior and its class, if it has one with a vtable in the module.
pub fn behavior(e: *mut Entity) -> Option<(*mut BehaviorAppBase, Arc<ClassInfo>)> {
    // SAFETY: entities come from the live list; the behavior pointer is null-checked before its vtable read.
    let b = unsafe { addr_of!((*e).behavior).read() };
    let vt = unsafe { b.as_ref()? }.vtable as usize;
    Some((b, class_of_vtable(vt)?))
}

/// The entity's behavior as a `BehaviorAppBase` (characters, enemies, NPCs).
pub fn as_app_base(e: *mut Entity) -> Option<*mut BehaviorAppBase> {
    behavior(e).filter(|(_, c)| c.is_app_base).map(|(b, _)| b)
}

/// The entity's behavior as a `Pl0000` (exact vtable match, so the struct size is known).
pub fn as_pl0000(e: *mut Entity) -> Option<*mut Pl0000> {
    behavior(e).filter(|(_, c)| c.is_pl0000).map(|(b, _)| b.cast())
}

/// The controlled character, if it is a `Pl0000` (not while hacking or in a flight unit).
pub fn player() -> Option<(u32, *mut Pl0000)> {
    let h = possessed_handle();
    Some((h, as_pl0000(entity_by_handle(h)?)?))
}

// -------------------------------------------------------------------------------------------------------
// Script functions

static SCRIPTS: LazyLock<Mutex<HashMap<&'static str, usize>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Native function behind a script function such as "Behavior.setSuspend". The list is built by the game
/// at startup, so this is looked up lazily and cached.
pub fn script_fn(name: &'static str) -> Option<usize> {
    if let Some(&f) = SCRIPTS.lock().ok()?.get(name) {
        return Some(f);
    }
    let g = game();
    let want = sdk::crc32(name.as_bytes());
    // SAFETY: resolved global pointer in the module.
    let head = unsafe { (g.addr.abs(g.addr.scripts) as *const *const ScriptFunction).read_volatile() };
    let mut node = head;
    // Nodes are static objects in the image; anything else means the list is not what we think.
    for _ in 0..100_000 {
        let n = node as usize;
        if node.is_null() || !g.in_module(n) || !g.in_module(n + size_of::<ScriptFunction>() - 1) {
            break;
        }
        // SAFETY: the whole node lies inside the module.
        let f = unsafe { &*node };
        if f.crc == want {
            if !g.in_module(f.function) {
                break;
            }
            SCRIPTS.lock().ok()?.insert(name, f.function);
            return Some(f.function);
        }
        node = f.next;
        if node == head {
            break;
        }
    }
    tracing::error!("script function {name} not found");
    None
}

/// Calls a script function that takes only `this`.
pub fn call_this(name: &'static str, this: *mut core::ffi::c_void) -> bool {
    let Some(f) = script_fn(name) else { return false };
    // SAFETY: native script functions of this kind are `fn(this)`; `this` is a validated behavior.
    unsafe { std::mem::transmute::<usize, unsafe extern "system" fn(*mut core::ffi::c_void)>(f)(this) };
    true
}

/// `Behavior.setSuspend(this, int)`.
pub fn set_suspend(this: *mut core::ffi::c_void, on: bool) -> bool {
    let Some(f) = script_fn("Behavior.setSuspend") else { return false };
    // SAFETY: as above, with the documented int argument.
    unsafe { std::mem::transmute::<usize, unsafe extern "system" fn(*mut core::ffi::c_void, i32)>(f)(this, on as i32) };
    true
}

/// Removes an entity from the world (`Behavior.terminate`).
pub fn terminate(b: *mut BehaviorAppBase) -> bool {
    call_this("Behavior.terminate", b.cast())
}

/// `fn(name, *mut Entity)`: gives an entity an AI routine.
pub fn assign_routine(e: *mut Entity, routine: &core::ffi::CStr) {
    let g = game();
    // SAFETY: resolved function with this signature (nier_coop / AutomataMP `Entity::assign_ai_routine`).
    unsafe {
        std::mem::transmute::<usize, unsafe extern "system" fn(*const u8, *mut Entity)>(g.addr.abs(g.addr.routine))(
            routine.as_ptr().cast(),
            e,
        )
    }
}

// -------------------------------------------------------------------------------------------------------
// Fields

/// Reads a `T` at a byte offset of a validated `Pl0000`, for fields not worth a struct member.
pub fn pl_read<T: Copy>(pl: *mut Pl0000, off: usize) -> T {
    assert!(!pl.is_null() && off + size_of::<T>() <= size_of::<Pl0000>());
    // SAFETY: in bounds of the Pl0000, checked above.
    unsafe { (pl as *const u8).add(off).cast::<T>().read_unaligned() }
}

/// The remaining-cooldown slot of the active pod; None if the index is out of range.
pub fn pod_cooldown_slot(pl: *mut Pl0000) -> Option<*mut f32> {
    // SAFETY: field of a validated Pl0000; the index is bounds-checked against the 3-entry array.
    unsafe {
        let i = addr_of!((*pl).pod_index).read() as usize;
        (i < 3).then(|| addr_of_mut!((*pl).pod_cooldown[i][1]))
    }
}

/// Camera world transform and vertical fov (AutomataMP `CameraGame.cpp`).
pub fn camera() -> Option<([[f32; 4]; 4], f32)> {
    let g = game();
    let cam = g.addr.abs(g.addr.camera) as *const CameraGame;
    // SAFETY: the camera object is a global inside the module; `data` is null-checked before use.
    unsafe {
        let data = addr_of!((*cam).data).read();
        let fov = addr_of!((*cam).fov).read();
        let view = addr_of!((*data.as_ref()?).view).read();
        (fov.is_finite() && fov > 0.01 && fov < 3.0 && view.iter().flatten().all(|f| f.is_finite())).then_some((view, fov))
    }
}

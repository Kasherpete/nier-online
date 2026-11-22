//! Enemy sync: the host owns enemies, the client mirrors them. Design from AutomataMP
//! `src/mods/multiplayer/EntitySync.cpp` and `MidHooks.cpp` (`on_entity_spawn`), and its Go server's
//! `SpawnEntity` / `EntityData` handlers.
//!
//! - Host: every networkable entity gets a net id. Spawns are announced with `SpawnEntity`, state is sent
//!   at 20 Hz in `Datagram::Entities`, and removal or death is sent as `DestroyEntity`.
//! - Client: networkable entities the client's own game spawns are deleted. `SpawnEntity` spawns a copy
//!   through the game's spawn function, and snapshots drive its position, rotation and HP.
//! - Damage: when a client copy loses HP locally, the client sends `HitRequest` and restores the host's
//!   value; the host applies it, and its next snapshot carries the result.
//!
//! EXPERIMENTAL and off by default (`enemy_sync` in the settings): not yet verified in-game. Known gaps:
//! client copies still run their own AI between snapshots (so they may attack locally), enemy animations
//! are not replayed, and death is driven only by the host's HP and `DestroyEntity`.

use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, CString, c_void};
use std::ptr::{addr_of, addr_of_mut};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use crossbeam_queue::ArrayQueue;

use crate::hooks::{self, game};
use crate::net::protocol::{self, Datagram, EntityState, Event, SpawnEntity};
use crate::sdk::{BehaviorAppBase, Entity, SpawnMatrix, SpawnParams, cVec4};
use crate::state::{self, Q, Role};
use crate::sync::{interp, world};

/// Host snapshot rate.
const SEND_EVERY_MS: f64 = 50.0;
/// Client: retries for a spawn whose assets may still be loading.
const SPAWN_ATTEMPTS: u32 = 5;
const SPAWN_RETRY_MS: f64 = 1000.0;
/// Client: don't terminate the same entity again before this.
const TERMINATE_RETRY_MS: f64 = 2000.0;

/// Set while the client spawns an entity the host asked for, so the spawn hook lets it live.
static HOST_REQUESTED: AtomicBool = AtomicBool::new(false);
/// Spawn hook -> tick: networkable entities the host's game just spawned.
static SPAWNED: LazyLock<ArrayQueue<SpawnEntity>> = LazyLock::new(|| ArrayQueue::new(1024));

/// Reads what we need from a spawn call. `params` comes from the game.
fn read_spawn(params: *const SpawnParams, handle: u32) -> Option<SpawnEntity> {
    // SAFETY: the game passes a valid SpawnParams; the name and matrix pointers are null-checked.
    unsafe {
        let p = params.as_ref()?;
        if p.name.is_null() {
            return None;
        }
        let name = CStr::from_ptr(p.name.cast()).to_bytes();
        let name = String::from_utf8(name.get(..name.len().min(protocol::MAX_ENTITY_NAME))?.to_vec()).ok()?;
        let (pos, rot) = match p.matrix.as_ref() {
            Some(m) => (m.position, m.rotation),
            None => (cVec4::default(), cVec4::default()),
        };
        Some(SpawnEntity { net_id: handle, name, model: p.model, pos, rot })
    }
}

/// Entity spawn detour. May run on any game thread: it only reads the call and queues a record.
pub fn on_spawn(spawner: *mut c_void, params: *const SpawnParams) -> *mut Entity {
    let e = hooks::call_spawn(spawner, params);
    if e.is_null() || HOST_REQUESTED.load(Relaxed) || state::role() != Role::Host || !state::ENEMY_SYNC.load(Relaxed) {
        return e;
    }
    if world::behavior(e).is_some_and(|(_, c)| c.is_networkable && c.is_app_base) {
        // `net_id` carries the handle until the tick assigns a real id.
        if let Some(s) = read_spawn(params, world::entity_handle(e)) {
            let _ = SPAWNED.force_push(s);
        }
    }
    e
}

struct Tracked {
    handle: u32,
    /// Client: latest host state.
    target: Option<EntityState>,
    /// Client: HP we last wrote, to detect local damage.
    applied_hp: u32,
}

struct PendingSpawn {
    spawn: SpawnEntity,
    attempts: u32,
    next_try_ms: f64,
}

#[derive(Default)]
pub struct EntitySync {
    role: Option<Role>,
    next_id: u32,
    tracked: HashMap<u32, Tracked>,
    by_handle: HashMap<u32, u32>,
    pending: VecDeque<PendingSpawn>,
    terminated: HashMap<u32, f64>,
    last_send_ms: f64,
    seq: u32,
}

fn networkable(e: *mut Entity) -> Option<*mut BehaviorAppBase> {
    world::behavior(e).filter(|(_, c)| c.is_networkable && c.is_app_base).map(|(b, _)| b)
}

fn read_state(b: *mut BehaviorAppBase) -> (cVec4, cVec4, u32) {
    // SAFETY: validated BehaviorAppBase.
    unsafe { (addr_of!((*b).position).read(), addr_of!((*b).rotation).read(), addr_of!((*b).health).read()) }
}

impl EntitySync {
    pub fn start(&mut self, role: Role) {
        *self = Self { role: Some(role), next_id: 1, ..Default::default() };
        while SPAWNED.pop().is_some() {}
        if role == Role::Host {
            // Late join: announce everything already alive (AutomataMP `on_enter_server`).
            for (h, e) in world::entities() {
                if let Some(b) = networkable(e) {
                    let (pos, rot, _) = read_state(b);
                    self.host_register(SpawnEntity { net_id: h, name: world::entity_name(e), model: model_of(b), pos, rot });
                }
            }
            tracing::info!("enemy sync: announced {} live entities", self.tracked.len());
        }
    }

    pub fn end(&mut self) {
        *self = Self::default();
    }

    fn host_register(&mut self, mut s: SpawnEntity) {
        let handle = s.net_id;
        if self.by_handle.contains_key(&handle) || s.name.is_empty() {
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        s.net_id = id;
        self.tracked.insert(id, Tracked { handle, target: None, applied_hp: 0 });
        self.by_handle.insert(handle, id);
        state::send_event(Event::SpawnEntity(s));
    }

    pub fn on_event(&mut self, e: Event) {
        match (self.role, e) {
            (Some(Role::Client), Event::SpawnEntity(s)) => {
                if !self.tracked.contains_key(&s.net_id) {
                    self.pending.push_back(PendingSpawn { spawn: s, attempts: 0, next_try_ms: 0.0 });
                }
            }
            (Some(Role::Client), Event::DestroyEntity { net_id }) => {
                self.pending.retain(|p| p.spawn.net_id != net_id);
                if let Some(t) = self.tracked.remove(&net_id) {
                    self.by_handle.remove(&t.handle);
                    if let Some(b) = world::entity_by_handle(t.handle).and_then(world::as_app_base) {
                        world::terminate(b);
                    }
                }
            }
            (Some(Role::Host), Event::HitRequest { net_id, damage }) => {
                if let Some(b) = self.tracked.get(&net_id).and_then(|t| world::entity_by_handle(t.handle)).and_then(world::as_app_base) {
                    // SAFETY: validated BehaviorAppBase.
                    unsafe {
                        let hp = addr_of_mut!((*b).health);
                        hp.write(hp.read().saturating_sub(damage));
                    }
                }
            }
            _ => {}
        }
    }

    pub fn on_batch(&mut self, items: Vec<EntityState>) {
        if self.role != Some(Role::Client) {
            return;
        }
        for s in items {
            if let Some(t) = self.tracked.get_mut(&s.net_id) {
                t.target = Some(s);
            }
        }
    }

    pub fn tick(&mut self, now: f64, dt: f32) {
        match self.role {
            Some(Role::Host) => self.tick_host(now),
            Some(Role::Client) => self.tick_client(now, dt),
            _ => {}
        }
    }

    fn tick_host(&mut self, now: f64) {
        while let Some(s) = SPAWNED.pop() {
            self.host_register(s);
        }
        if now - self.last_send_ms < SEND_EVERY_MS {
            return;
        }
        self.last_send_ms = now;
        let mut items = Vec::with_capacity(self.tracked.len());
        let mut gone = Vec::new();
        for (&id, t) in &self.tracked {
            match world::entity_by_handle(t.handle).and_then(world::as_app_base) {
                Some(b) => {
                    let (pos, rot, hp) = read_state(b);
                    if hp == 0 {
                        gone.push(id);
                    } else if pos.is_sane() && rot.is_sane() {
                        items.push(EntityState { net_id: id, pos, rot, hp });
                    }
                }
                None => gone.push(id),
            }
        }
        for id in gone {
            if let Some(t) = self.tracked.remove(&id) {
                self.by_handle.remove(&t.handle);
            }
            state::send_event(Event::DestroyEntity { net_id: id });
        }
        for chunk in items.chunks(protocol::ENTITIES_PER_DATAGRAM) {
            self.seq = self.seq.wrapping_add(1);
            let d = Datagram::Entities { seq: self.seq, time_ms: now as u32, items: chunk.to_vec() };
            let _ = Q.out_datagrams.force_push(d);
        }
    }

    fn tick_client(&mut self, now: f64, dt: f32) {
        // Delete networkable entities our own game spawned (AutomataMP `EntitySync::think`).
        self.terminated.retain(|_, t| now - *t < TERMINATE_RETRY_MS);
        for (h, e) in world::entities() {
            if self.by_handle.contains_key(&h) || self.terminated.contains_key(&h) {
                continue;
            }
            if let Some(b) = networkable(e) {
                world::terminate(b);
                self.terminated.insert(h, now);
            }
        }

        // Spawn what the host asked for.
        for _ in 0..self.pending.len() {
            let Some(mut p) = self.pending.pop_front() else { break };
            if now < p.next_try_ms {
                self.pending.push_back(p);
                continue;
            }
            match spawn_copy(&p.spawn) {
                Some(h) => {
                    self.tracked.insert(p.spawn.net_id, Tracked { handle: h, target: None, applied_hp: 0 });
                    self.by_handle.insert(h, p.spawn.net_id);
                }
                None if p.attempts + 1 < SPAWN_ATTEMPTS => {
                    p.attempts += 1;
                    p.next_try_ms = now + SPAWN_RETRY_MS;
                    self.pending.push_back(p);
                }
                None => tracing::warn!("enemy sync: could not spawn {} (model {:#x})", p.spawn.name, p.spawn.model),
            }
        }

        // Drive the copies and report local damage.
        for (&id, t) in &mut self.tracked {
            let (Some(target), Some(b)) = (t.target, world::entity_by_handle(t.handle).and_then(world::as_app_base)) else {
                continue;
            };
            // SAFETY: validated BehaviorAppBase.
            unsafe {
                let hp = addr_of_mut!((*b).health);
                let local = hp.read();
                if t.applied_hp != 0 && local < t.applied_hp {
                    state::send_event(Event::HitRequest { net_id: id, damage: t.applied_hp - local });
                }
                hp.write(target.hp);
                t.applied_hp = target.hp;
                let pos = addr_of_mut!((*b).position);
                pos.write(interp::correct(pos.read(), target.pos, dt, 5.0));
                addr_of_mut!((*b).rotation).write(target.rot);
            }
        }
    }

    pub fn net_id_of(&self, handle: u32) -> Option<u32> {
        self.by_handle.get(&handle).copied()
    }
}

fn model_of(b: *mut BehaviorAppBase) -> u32 {
    // SAFETY: validated BehaviorAppBase.
    unsafe { addr_of!((*b).model_index).read() }
}

/// Client: spawns a copy of a host entity through the game's own spawn function.
fn spawn_copy(s: &SpawnEntity) -> Option<u32> {
    let name = CString::new(s.name.as_bytes()).ok()?;
    let m = SpawnMatrix::at(s.pos, s.rot);
    let params = SpawnParams::new(&name, s.model, &m);
    let g = game();
    HOST_REQUESTED.store(true, Relaxed);
    let e = hooks::call_spawn(g.addr.abs(g.addr.spawner) as *mut c_void, &params);
    HOST_REQUESTED.store(false, Relaxed);
    if e.is_null() {
        return None;
    }
    if let Some(b) = world::as_app_base(e) {
        world::set_suspend(b.cast(), false);
    }
    Some(world::entity_handle(e))
}

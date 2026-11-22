//! Game-side sync logic, run once per frame from the update hook on the game thread. Drains the queues the
//! net thread fills, updates the remote player and (optionally) enemies, and publishes overlay data.

pub mod aggro;
pub mod entities;
pub mod events;
pub mod interp;
pub mod player;
pub mod world;

use std::ptr::addr_of;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{LazyLock, Mutex};

use crate::net::protocol::Event;
use crate::state::{self, EntityInfo, GameCommand, Inspect, Q, Role};
use entities::EntitySync;
use player::{PlayerSync, RemoteEvent};

const OVERLAY_EVERY_MS: f64 = 16.0;
const ENTITY_LIST_EVERY_MS: f64 = 250.0;

struct SyncState {
    player: PlayerSync,
    entities: EntitySync,
    events: events::Events,
    active: bool,
    enemy_sync: bool,
    last_tick_ms: f64,
    last_overlay_ms: f64,
    last_list_ms: f64,
}

static SYNC: LazyLock<Mutex<SyncState>> = LazyLock::new(|| {
    Mutex::new(SyncState {
        player: PlayerSync::new(),
        entities: EntitySync::default(),
        events: events::Events::default(),
        active: false,
        enemy_sync: false,
        last_tick_ms: 0.0,
        last_overlay_ms: 0.0,
        last_list_ms: 0.0,
    })
});

/// The per-frame tick. Called from the update hook.
pub fn tick() {
    // Never block the game thread; a re-entrant call (a game function we call runs the update) is skipped.
    let Ok(mut st) = SYNC.try_lock() else { return };
    let st = &mut *st;
    let now = state::now_ms();
    let dt = ((now - st.last_tick_ms) / 1000.0).clamp(0.0, 0.25) as f32;
    st.last_tick_ms = now;

    while let Some(c) = Q.game_cmds.pop() {
        match c {
            GameCommand::SessionStarted { role, enemy_sync, peer_name } => {
                tracing::info!("session started as {role:?} with {peer_name} (enemy sync {enemy_sync})");
                st.active = true;
                st.enemy_sync = enemy_sync;
                st.player.reset();
                st.events.start(role);
                aggro::reset();
                aggro::set_enabled(crate::config::get().enemy_target_client);
                if enemy_sync && role != Role::None {
                    st.entities.start(role);
                    st.entities.set_replay_unverified(crate::config::get().enemy_replay_unverified);
                }
            }
            GameCommand::SessionEnded => {
                tracing::info!("session ended; single-player behavior restored");
                st.active = false;
                st.entities.end();
                st.events.end();
                aggro::reset();
            }
        }
    }

    while let Some(r) = Q.in_player.pop() {
        st.events.on_peer_snapshot(r.value);
        st.player.on_snapshot(r.at_ms, r.value);
    }
    while let Some(r) = Q.in_entities.pop() {
        st.entities.on_batch(r.at_ms, r.value);
    }
    while let Some(e) = Q.in_events.pop() {
        match e {
            Event::Anim { time_ms, args } => st.player.timeline.push_event(time_ms, RemoteEvent::Anim(args)),
            Event::Buttons { time_ms, buttons } => st.player.timeline.push_event(time_ms, RemoteEvent::Buttons(buttons)),
            e @ (Event::SpawnEntity(_) | Event::EntityAnim { .. } | Event::EntityDied { .. } | Event::DestroyEntity { .. }) => {
                st.entities.on_event(e, now)
            }
            e @ (Event::PlayerDied | Event::Cutscene { .. }) => st.events.on_event(e, now),
            _ => {}
        }
    }

    st.player.tick(now, dt, st.active);
    st.events.tick(now);
    if st.active && st.enemy_sync {
        st.entities.tick(now, dt);
    }

    if now - st.last_overlay_ms >= OVERLAY_EVERY_MS {
        st.last_overlay_ms = now;
        publish_overlay(st, now);
    }
}

fn publish_overlay(st: &mut SyncState, now: f64) {
    let remote = st.player.remote_ptr();
    let want_list = state::DEBUG_LIST_WANTED.load(Relaxed) && now - st.last_list_ms >= ENTITY_LIST_EVERY_MS;
    let list = want_list.then(|| {
        st.last_list_ms = now;
        entity_list(&st.entities)
    });
    let inspect = match state::DEBUG_INSPECT.load(Relaxed) {
        0 => None,
        h => inspect(h),
    };
    let Ok(mut o) = state::OVERLAY.try_lock() else { return };
    o.game_ok = true;
    o.local_player = world::player().is_some();
    o.camera = world::camera();
    o.remote_head = remote.map(player::head_position);
    // SAFETY: validated remote Pl0000.
    o.remote_hp = remote.map(|b| unsafe { addr_of!((*b).base.health).read() });
    if let Some(l) = list {
        o.entities = l;
    }
    o.inspect = inspect;
    o.enemy = (st.active && st.enemy_sync).then(|| st.entities.stats());
}

fn entity_list(es: &EntitySync) -> Vec<EntityInfo> {
    world::entities()
        .into_iter()
        .filter_map(|(h, e)| {
            let (b, class) = world::behavior(e)?;
            let (pos, hp) = if class.is_app_base {
                // SAFETY: validated BehaviorAppBase.
                unsafe { (addr_of!((*b).position).read(), addr_of!((*b).health).read()) }
            } else {
                (Default::default(), 0)
            };
            Some(EntityInfo {
                handle: h,
                name: world::entity_name(e),
                class: class.short_name(),
                pos: [pos.x, pos.y, pos.z],
                hp,
                net: es.net_id_of(h),
            })
        })
        .collect()
}

/// Dump of the first bytes of an entity's behavior, bounded by the size its class is known to have.
fn inspect(handle: u32) -> Option<Inspect> {
    let e = world::entity_by_handle(handle)?;
    let (b, class) = world::behavior(e)?;
    let len = if class.is_pl0000 {
        size_of::<crate::sdk::Pl0000>()
    } else if class.is_app_base {
        size_of::<crate::sdk::BehaviorAppBase>()
    } else {
        0x50 // vtable + Model header only
    };
    let len = len.min(0x2000);
    let rows = (0..len / 4)
        .map(|i| {
            // SAFETY: inside the object's known size.
            let v = unsafe { (b as *const u8).add(i * 4).cast::<u32>().read_unaligned() };
            (i * 4, v, f32::from_bits(v))
        })
        .collect();
    Some(Inspect { handle, class_chain: class.chain.clone(), rows })
}

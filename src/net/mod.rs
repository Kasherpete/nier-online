//! The network thread: a single-threaded tokio runtime that owns the iroh endpoint. It never touches game
//! memory; it only moves data between the shared queues in `crate::state` and the peer.
//!
//! One QUIC connection per session carries:
//! - unreliable datagrams: player snapshots (every frame) and enemy batches (20 Hz), latest-wins;
//! - one reliable bidirectional stream: the `Hello` handshake, then every event in order.
//!
//! iroh connects directly through UDP hole punching when it can, and through a relay when it can't.

pub mod protocol;
pub mod save;
pub mod ticket;

use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use bytes::Bytes;
use iroh::endpoint::{Connection, QuicTransportConfig, RecvStream, SendStream, presets};
use iroh::{Endpoint, RelayMode};
use tokio::time::{Instant, interval, sleep, timeout};

use crate::config;
use crate::state::{self, GameCommand, NetCommand, Q, Received, Role};
use protocol::{ALPN, Datagram, Event, Hello, MAX_FRAME, PROTOCOL_VERSION};

/// Crate version plus the git commit it was built from ("-dirty" with uncommitted changes, see `build.rs`).
/// Both players must match exactly.
pub const MOD_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("NIER_ONLINE_COMMIT"));
/// When this DLL was built (shown in the menu and log, not compared).
pub const BUILT: &str = env!("NIER_ONLINE_BUILT");
/// Snapshot send interval (one per game frame at 60 fps).
const SEND_EVERY: Duration = Duration::from_millis(16);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Close the connection after this long without hearing from the peer (keep-alives included).
const IDLE_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a client keeps trying to reach the host (a first join, a restart into its world, or after a drop).
const RECONNECT_FOR: Duration = Duration::from_secs(90);

/// Game build identifier sent in `Hello`; set at startup from the exe's PE timestamp.
static GAME_BUILD: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub fn spawn(game_build: u32) {
    GAME_BUILD.store(game_build, Relaxed);
    std::thread::Builder::new()
        .name("nier_online net".into())
        .spawn(|| {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!("could not start the network runtime: {e}");
                    return;
                }
            };
            rt.block_on(main_loop());
        })
        .expect("spawn net thread");
}

fn status(f: impl FnOnce(&mut state::NetStatus)) {
    state::net_status(f);
}

fn set_idle(error: Option<String>) {
    status(|s| {
        *s = state::NetStatus { state: "Idle".into(), last_error: error, ..Default::default() };
    });
}

async fn next_command() -> NetCommand {
    loop {
        if let Some(c) = Q.net_cmds.pop() {
            return c;
        }
        sleep(Duration::from_millis(20)).await;
    }
}

async fn endpoint() -> Result<Endpoint, String> {
    let cfg = config::get();
    let transport = QuicTransportConfig::builder()
        .keep_alive_interval(Duration::from_secs(2))
        .max_idle_timeout(IDLE_TIMEOUT.try_into().ok())
        .build();
    let mut b = Endpoint::builder(presets::N0).alpns(vec![ALPN.to_vec()]).transport_config(transport);
    if !cfg.relay_url.trim().is_empty() {
        let url = cfg.relay_url.trim().parse().map_err(|e| format!("bad relay_url: {e}"))?;
        b = b.relay_mode(RelayMode::custom([url]));
    }
    b.bind().await.map_err(|e| format!("could not open the network endpoint: {e}"))
}

async fn main_loop() {
    let mut ep: Option<Endpoint> = None;
    let mut cmd = None;
    loop {
        let c = match cmd.take() {
            Some(c) => c,
            None => next_command().await,
        };
        let result = match c {
            NetCommand::Loopback => {
                cmd = loopback().await;
                Ok(())
            }
            NetCommand::Host(_) | NetCommand::Join(_) => {
                if ep.is_none() {
                    status(|s| {
                        s.state = "Starting network...".into();
                        s.busy = true;
                    });
                    match endpoint().await {
                        Ok(e) => ep = Some(e),
                        Err(e) => {
                            set_idle(Some(e));
                            continue;
                        }
                    }
                }
                let e = ep.as_ref().unwrap();
                match c {
                    NetCommand::Host(slot) => {
                        save::set_shared(slot);
                        tracing::info!("hosting; sharing {}", slot.map_or("the newest save (new game)".into(), |s| format!("slot {}", s + 1)));
                        host(e).await.map(|next| cmd = next)
                    }
                    NetCommand::Join(code) => {
                        let r = join(e, &code).await.map(|next| cmd = next);
                        // Not joining any more: a held save read goes ahead with the saves we have.
                        crate::hooks::savefile::hold_next_read(false);
                        r
                    }
                    _ => unreachable!(),
                }
            }
            NetCommand::Disconnect | NetCommand::Chat(_) => Ok(()),
        };
        end_session();
        set_idle(result.err());
    }
}

/// Waits for one client. Returns the command that ended it (if any) to run next.
async fn host(ep: &Endpoint) -> Result<Option<NetCommand>, String> {
    status(|s| {
        s.state = "Waiting for the relay...".into();
        s.busy = true;
    });
    // So the join code includes the relay, which makes it dialable from anywhere.
    let _ = timeout(Duration::from_secs(10), ep.online()).await;
    let code = ticket::encode(&ep.addr());
    status(|s| {
        s.state = "Hosting: waiting for a player to join".into();
        s.join_code = Some(code.clone());
        s.busy = false;
    });
    loop {
        tokio::select! {
            incoming = ep.accept() => {
                let Some(incoming) = incoming else { return Err("network endpoint closed".into()) };
                let conn = match incoming.await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!("incoming connection failed: {e}");
                        continue;
                    }
                };
                let (send, recv) = match timeout(HANDSHAKE_TIMEOUT, conn.accept_bi()).await {
                    Ok(Ok(s)) => s,
                    _ => {
                        conn.close(1u32.into(), b"no handshake");
                        continue;
                    }
                };
                // Refuse anyone else while this session runs.
                let refuser = {
                    let ep = ep.clone();
                    tokio::spawn(async move {
                        while let Some(i) = ep.accept().await {
                            i.refuse();
                        }
                    })
                };
                let r = session(conn, send, recv, Role::Host).await;
                refuser.abort();
                match r {
                    Ok(next) => return Ok(next),
                    Err(e) => {
                        // The client dropped (or is restarting into this world): keep hosting with the same
                        // code, so it can come back.
                        end_session();
                        tracing::warn!("client left ({e}); waiting for a player to join again");
                        status(|s| {
                            *s = state::NetStatus {
                                state: "Hosting: waiting for a player to join".into(),
                                join_code: Some(code.clone()),
                                last_error: Some(format!("last session: {e}")),
                                ..Default::default()
                            };
                        });
                    }
                }
            }
            c = next_command() => return match c {
                NetCommand::Disconnect | NetCommand::Chat(_) => Ok(None),
                other => Ok(Some(other)),
            },
        }
    }
}

/// Connects to a host, retrying for up to `RECONNECT_FOR`: after a restart into the host's world the host may
/// still be timing out the session of the game copy that just closed. After a session drops, it reconnects to
/// the same code for that long too.
async fn join(ep: &Endpoint, code: &str) -> Result<Option<NetCommand>, String> {
    let addr = ticket::decode(code)?;
    let mut retry_since = Some(Instant::now());
    status(|s| s.joined_code = Some(code.trim().to_string()));
    loop {
        status(|s| {
            s.state = if retry_since.is_some() { "Connecting (retrying)...".into() } else { "Connecting...".into() };
            s.busy = true;
        });
        let attempt = tokio::select! {
            r = timeout(CONNECT_TIMEOUT, ep.connect(addr.clone(), ALPN)) => match r {
                Ok(Ok(c)) => Ok(c),
                Ok(Err(e)) => Err(format!("could not connect: {e}")),
                Err(_) => Err("could not connect: timed out".into()),
            },
            c = next_command() => return match c {
                NetCommand::Disconnect | NetCommand::Chat(_) => Ok(None),
                other => Ok(Some(other)),
            },
        };
        let conn = match attempt {
            Ok(c) => c,
            Err(e) => match retry_since {
                Some(t) if t.elapsed() < RECONNECT_FOR => {
                    tracing::info!("{e}; retrying");
                    tokio::select! {
                        _ = sleep(Duration::from_secs(3)) => continue,
                        c = next_command() => return match c {
                            NetCommand::Disconnect | NetCommand::Chat(_) => Ok(None),
                            other => Ok(Some(other)),
                        },
                    }
                }
                _ => return Err(e),
            },
        };
        let (send, recv) = conn.open_bi().await.map_err(|e| format!("could not open a stream: {e}"))?;
        let r = session(conn, send, recv, Role::Client).await;
        // Role is set only after a good handshake: a refused or mismatched handshake is not retried.
        let was_connected = state::role() != Role::None;
        match r {
            Err(e) if was_connected => {
                end_session();
                tracing::warn!("lost the host ({e}); reconnecting for up to {} s", RECONNECT_FOR.as_secs());
                retry_since = Some(Instant::now());
                status(|s| s.last_error = Some(format!("lost the host: {e}")));
            }
            other => return other,
        }
    }
}

async fn write_frame(send: &mut SendStream, e: &Event) -> Result<(), String> {
    let body = protocol::encode(e);
    if body.len() > MAX_FRAME {
        return Err("event too large".into());
    }
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    send.write_all(&frame).await.map_err(|e| format!("send failed: {e}"))
}

async fn read_frame(recv: &mut RecvStream) -> Result<Event, String> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await.map_err(|e| format!("peer closed the stream: {e}"))?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(format!("peer sent an oversized frame ({len} bytes)"));
    }
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body).await.map_err(|e| format!("peer closed the stream: {e}"))?;
    protocol::decode_event(&body).map_err(|e| format!("peer sent a bad event: {e}"))
}

fn our_hello(role: Role) -> Hello {
    let cfg = config::get();
    let mut name = cfg.player_name.trim().to_string();
    name.truncate(protocol::MAX_NAME);
    Hello {
        protocol: PROTOCOL_VERSION,
        mod_version: MOD_VERSION.into(),
        game_build: GAME_BUILD.load(Relaxed),
        name,
        enemy_sync: role == Role::Host && cfg.enemy_sync,
    }
}

/// Why a peer's `Hello` is unacceptable, if it is.
fn mismatch(ours: &Hello, theirs: &Hello) -> Option<String> {
    if theirs.protocol != ours.protocol || theirs.mod_version != ours.mod_version {
        return Some(format!("mod version mismatch: you have {}, the other player has {}", ours.mod_version, theirs.mod_version));
    }
    if theirs.game_build != ours.game_build {
        return Some(format!(
            "game version mismatch (build {:08x} vs {:08x}): both players must update the game",
            ours.game_build, theirs.game_build
        ));
    }
    None
}

async fn handshake(send: &mut SendStream, recv: &mut RecvStream, role: Role) -> Result<Hello, String> {
    let ours = our_hello(role);
    match role {
        Role::Client => {
            write_frame(send, &Event::Hello(ours.clone())).await?;
            match read_frame(recv).await? {
                Event::Hello(h) => match mismatch(&ours, &h) {
                    Some(why) => Err(why),
                    None => Ok(h),
                },
                Event::Refuse { reason } => Err(format!("the host refused: {reason}")),
                _ => Err("unexpected handshake message".into()),
            }
        }
        _ => {
            let Event::Hello(h) = read_frame(recv).await? else { return Err("unexpected handshake message".into()) };
            if let Some(why) = mismatch(&ours, &h) {
                let _ = write_frame(send, &Event::Refuse { reason: why.clone() }).await;
                return Err(why);
            }
            write_frame(send, &Event::Hello(ours)).await?;
            Ok(h)
        }
    }
}

fn end_session() {
    if state::role() == Role::Client {
        crate::hooks::savefile::session_ended();
    }
    if state::role() != Role::None {
        state::set_role(Role::None);
        state::ENEMY_SYNC.store(false, Relaxed);
        let _ = Q.game_cmds.push(GameCommand::SessionEnded);
    }
    state::clear_session_queues();
}

fn path_status(conn: &Connection) -> (Option<f32>, Option<&'static str>) {
    let paths = conn.paths();
    let selected = paths.iter().find(|p| p.is_selected());
    match selected {
        Some(p) => (Some(p.rtt().as_secs_f32() * 1000.0), Some(if p.is_relay() { "relay" } else { "direct" })),
        None => (None, None),
    }
}

async fn session(conn: Connection, mut send: SendStream, mut recv: RecvStream, role: Role) -> Result<Option<NetCommand>, String> {
    status(|s| s.state = "Handshake...".into());
    let peer = match timeout(HANDSHAKE_TIMEOUT, handshake(&mut send, &mut recv, role)).await {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => {
            conn.close(2u32.into(), b"handshake failed");
            return Err(e);
        }
        Err(_) => {
            conn.close(2u32.into(), b"handshake timeout");
            return Err("handshake timed out".into());
        }
    };
    let enemy_sync = match role {
        Role::Host => our_hello(role).enemy_sync,
        _ => peer.enemy_sync,
    };
    tracing::info!("connected to {} (mod {}), enemy sync {enemy_sync}", peer.name, peer.mod_version);
    state::clear_session_queues();
    state::ENEMY_SYNC.store(enemy_sync, Relaxed);
    state::set_role(role);
    let _ = Q.game_cmds.push(GameCommand::SessionStarted { role, enemy_sync, peer_name: peer.name.clone() });
    status(|s| {
        s.state = format!("Connected ({})", if role == Role::Host { "host" } else { "client" });
        s.peer_name = Some(peer.name.clone());
        s.peer_version = Some(peer.mod_version.clone());
        s.connected = true;
        s.busy = false;
        s.last_error = None;
    });

    // Save sync: the client plays in the host's world while connected (`hooks::savefile`) and receives the
    // host's save on its own streams; the host sends it below.
    let dir = save::save_dir();
    if dir.is_none() {
        tracing::warn!("save sync: save folder (Documents\\My Games\\NieR_Automata) not found");
    }
    let saves_in = (role == Role::Client).then(|| {
        crate::hooks::savefile::enter_host_world();
        let (conn, dir) = (conn.clone(), dir.clone());
        tokio::spawn(async move {
            while let Ok(mut r) = conn.accept_uni().await {
                let mut h = [0u8; save::HEADER];
                if r.read_exact(&mut h).await.is_err() {
                    continue;
                }
                let (slot, crc) = match save::parse_header(&h) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("save sync: rejected a transfer: {e}");
                        continue;
                    }
                };
                let mut data = vec![0u8; crate::hooks::savefile::SAVE_SIZE];
                if let Err(e) = r.read_exact(&mut data).await {
                    tracing::warn!("save sync: transfer cut off: {e}");
                    continue;
                }
                match save::accept(slot, crc, data, dir.as_deref()) {
                    Ok(()) => {
                        tracing::info!("save sync: received the host's save (slot {})", slot + 1);
                        state::push_chat(format!("[co-op] Received the host's save (slot {}).", slot + 1));
                    }
                    Err(e) => tracing::warn!("save sync: {e}"),
                }
            }
        })
    });
    let mut last_save: Option<(u32, u32)> = None;
    let mut last_save_check = Instant::now() - Duration::from_secs(10);
    let mut save_out: Option<tokio::task::JoinHandle<()>> = None;
    let session_start = Instant::now();
    let mut no_save_noted = false;

    let peer_name = peer.name.clone();
    let mut events_in = tokio::spawn(async move {
        loop {
            match read_frame(&mut recv).await {
                Ok(Event::Chat { text }) => state::push_chat(format!("{peer_name}: {text}")),
                Ok(Event::Hello(_) | Event::Refuse { .. }) => {}
                Ok(e) => {
                    if Q.in_events.push(e).is_err() {
                        tracing::warn!("incoming event queue full; event dropped");
                    }
                }
                Err(e) => return e,
            }
        }
    });
    let datagrams_in = {
        let conn = conn.clone();
        tokio::spawn(async move {
            let (mut last_player, mut last_batch, mut bad) = (None::<u32>, 0u32, 0u32);
            while let Ok(d) = conn.read_datagram().await {
                let at_ms = state::now_ms();
                match protocol::decode_datagram(&d) {
                    Ok(Datagram::Player { seq, snap }) => {
                        // Drop reordered and duplicate datagrams (wrapping compare).
                        if last_player.is_none_or(|l| (seq.wrapping_sub(l) as i32) > 0) {
                            last_player = Some(seq);
                            let _ = Q.in_player.force_push(Received { at_ms, value: snap });
                        }
                    }
                    Ok(Datagram::Entities { time_ms, items, .. }) => {
                        // One tick's batch spans several datagrams with the same time; only older ticks are stale.
                        if time_ms >= last_batch {
                            last_batch = time_ms;
                            let _ = Q.in_entities.force_push(Received { at_ms, value: items });
                        }
                    }
                    Err(e) => {
                        bad += 1;
                        // The first few at warn (a whole snapshot stream can die this way), then quietly.
                        if bad <= 10 {
                            tracing::warn!("dropped bad datagram ({} bytes): {e}", d.len());
                        } else {
                            tracing::debug!("dropped bad datagram: {e}");
                        }
                    }
                }
            }
        })
    };

    let mut tick = interval(SEND_EVERY);
    let mut seq = 0u32;
    let mut send_errors = 0u32;
    let mut last_status = Instant::now();
    let (mut last_counts, mut loss) = ((0u64, 0u64), 0.0f32);
    let result = loop {
        tokio::select! {
            _ = tick.tick() => {}
            r = &mut events_in => break Err(r.unwrap_or_else(|e| e.to_string())),
            _ = conn.closed() => break Err("connection closed".into()),
        }
        // Snapshots: always send the latest, even if unchanged, so a paused game still looks alive.
        if let Some(snap) = state::LOCAL_PLAYER.load() {
            seq = seq.wrapping_add(1);
            let _ = conn.send_datagram(Bytes::from(protocol::encode(&Datagram::Player { seq, snap })));
        }
        while let Some(d) = Q.out_datagrams.pop() {
            let b = protocol::encode(&d);
            let len = b.len();
            let r = if len <= protocol::MAX_DATAGRAM { conn.send_datagram(Bytes::from(b)).map_err(|e| e.to_string()) } else { Err("too large".into()) };
            if let Err(e) = r {
                send_errors += 1;
                if send_errors <= 10 {
                    tracing::warn!("could not send a {len}-byte datagram: {e}");
                }
            }
        }
        let mut failed = None;
        while let Some(e) = Q.out_events.pop() {
            if let Err(err) = write_frame(&mut send, &e).await {
                failed = Some(err);
                break;
            }
        }
        if let Some(e) = failed {
            break Err(e);
        }
        while let Some(c) = Q.net_cmds.pop() {
            match c {
                NetCommand::Chat(text) => {
                    let mut text = text.trim().to_string();
                    text.truncate(protocol::MAX_CHAT);
                    if !text.is_empty() && !text.chars().any(char::is_control) {
                        state::push_chat(format!("{}: {text}", our_hello(role).name));
                        let _ = write_frame(&mut send, &Event::Chat { text }).await;
                    }
                }
                NetCommand::Disconnect => {
                    conn.close(0u32.into(), b"bye");
                    datagrams_in.abort();
                    for t in saves_in.iter().chain(save_out.iter()) {
                        t.abort();
                    }
                    events_in.abort();
                    return Ok(None);
                }
                other => {
                    conn.close(0u32.into(), b"bye");
                    datagrams_in.abort();
                    for t in saves_in.iter().chain(save_out.iter()) {
                        t.abort();
                    }
                    events_in.abort();
                    return Ok(Some(other));
                }
            }
        }
        if role == Role::Client
            && !no_save_noted
            && session_start.elapsed() > Duration::from_secs(15)
            && !crate::hooks::savefile::host_save().0
        {
            // A host on a new game that has not saved yet has nothing to send: don't hold the save list for it.
            no_save_noted = true;
            crate::hooks::savefile::hold_next_read(false);
            tracing::warn!("save sync: no save from the host after 15 s");
            state::push_chat("[co-op] The host has no save to send yet (a new game?). Once they save, use Restart into host's world.".into());
        }
        if role == Role::Host
            && last_save_check.elapsed() > Duration::from_secs(1)
            && save_out.as_ref().is_none_or(|t| t.is_finished())
            && let Some(dir) = &dir
        {
            last_save_check = Instant::now();
            if let Some((slot, crc, data)) = save::to_send(dir, save::shared().or(crate::hooks::savefile::loaded_slot()), last_save) {
                last_save = Some((slot, crc));
                tracing::info!("save sync: sending slot {} to the client", slot + 1);
                let conn = conn.clone();
                save_out = Some(tokio::spawn(async move {
                    let r = async {
                        let mut s = conn.open_uni().await.map_err(|e| e.to_string())?;
                        s.write_all(&save::header(slot, crc, data.len())).await.map_err(|e| e.to_string())?;
                        s.write_all(&data).await.map_err(|e| e.to_string())?;
                        s.finish().map_err(|e| e.to_string())
                    };
                    if let Err(e) = r.await {
                        tracing::warn!("save sync: sending failed: {e}");
                    }
                }));
            }
        }
        if last_status.elapsed() > Duration::from_millis(500) {
            last_status = Instant::now();
            let (rtt, path) = path_status(&conn);
            // Packet loss over this window, smoothed (iroh counts QUIC packets sent and declared lost).
            let st = conn.stats();
            let (sent, lost) = (st.sent_packets.saturating_sub(last_counts.0), st.lost_packets.saturating_sub(last_counts.1));
            last_counts = (st.sent_packets, st.lost_packets);
            if sent > 0 {
                loss = 0.8 * loss + 0.2 * (lost as f32 / sent as f32 * 100.0).min(100.0);
            }
            status(|s| {
                s.rtt_ms = rtt;
                s.path = path;
                s.loss_pct = Some(loss);
            });
        }
    };
    datagrams_in.abort();
    for t in saves_in.iter().chain(save_out.iter()) {
        t.abort();
    }
    events_in.abort();
    conn.close(0u32.into(), b"bye");
    tracing::warn!("session ended: {}", result.as_ref().err().map_or("", |e| e.as_str()));
    result.map(|()| None)
}

/// Mirror test: our own snapshots and events come back as the remote player's, shifted 2 m sideways, so
/// the remote-player code can be tested on one PC.
async fn loopback() -> Option<NetCommand> {
    state::clear_session_queues();
    state::set_role(Role::Client);
    let _ = Q.game_cmds.push(GameCommand::SessionStarted { role: Role::Client, enemy_sync: false, peer_name: "mirror".into() });
    status(|s| {
        *s = state::NetStatus {
            state: "Mirror test (loopback)".into(),
            peer_name: Some("mirror".into()),
            peer_version: Some(MOD_VERSION.into()),
            connected: true,
            ..Default::default()
        };
    });
    let mut tick = interval(SEND_EVERY);
    loop {
        tick.tick().await;
        if let Some(mut snap) = state::LOCAL_PLAYER.load() {
            snap.pos.x += 2.0;
            let _ = Q.in_player.force_push(Received { at_ms: state::now_ms(), value: snap });
        }
        while let Some(e) = Q.out_events.pop() {
            if matches!(e, Event::Anim { .. } | Event::Buttons { .. }) {
                let _ = Q.in_events.push(e);
            }
        }
        while Q.out_datagrams.pop().is_some() {}
        while let Some(c) = Q.net_cmds.pop() {
            match c {
                NetCommand::Chat(text) => state::push_chat(format!("mirror: {text}")),
                NetCommand::Disconnect => return None,
                other => return Some(other),
            }
        }
    }
}

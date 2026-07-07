//! WebSocket hub. One tokio task per connected client; broadcast/send_to
//! helpers enqueue messages (or raw media chunks) to each client's writer.

use std::net::SocketAddr;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMsg;

use cuemesh2_shared::protocol::{
    ClientMsg, ClientState, ControllerMsg, Envelope, HelloAck, Layer, LoadCue, ShowSync,
    PROTOCOL_VERSION,
};
use cuemesh2_shared::show::Cue;

use crate::state::{ClientRow, Outgoing, SharedState};

const OUTBOUND_QUEUE: usize = 64;

/// Bind and accept WebSocket clients forever.
pub async fn run(state: SharedState, bind: SocketAddr) -> Result<()> {
    let listener = TcpListener::bind(bind).await?;
    log(&state, format!("listening on {bind}"));
    serve(state, listener).await
}

/// Accept clients on an already-bound listener (integration tests bind to
/// port 0 themselves to learn the real address).
pub async fn serve(state: SharedState, listener: TcpListener) -> Result<()> {
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, addr, state.clone()).await {
                        log(&state, format!("client {addr}: {e}"));
                    }
                });
            }
            Err(e) => {
                log(&state, format!("accept error: {e}"));
            }
        }
    }
}

/// Build a SHOW_SYNC from the currently loaded show, if any.
pub fn show_sync_msg(state: &SharedState) -> Option<ControllerMsg> {
    let s = state.lock().unwrap();
    let show = s.show.as_ref()?;
    Some(ControllerMsg::ShowSync(ShowSync {
        title: show.show.title.clone(),
        dropout_policy: show.show.dropout_policy,
        sync: show.show.sync.clone(),
        default_fade_ms: show.show.settings.default_fade_ms,
        cues: show.cues.clone(),
    }))
}

/// A LOAD_CUE/STANDBY payload preloading `cue` onto `layer`.
pub fn load_cue_for(cue: &Cue, layer: Layer) -> LoadCue {
    LoadCue {
        cue_id: cue.id.clone(),
        layer,
        file: cue.file.clone(),
        kind: cue.kind,
        start_ms: None,
        end_ms: None,
        fade_in_ms: cue.fade_in_ms,
        fade_out_ms: cue.fade_out_ms,
        crossfade_to_next_ms: cue.crossfade_to_next_ms,
    }
}

/// Build a STANDBY for whatever cue is currently on standby, so a client that
/// joins after the controller already issued the standby still prerolls it.
pub fn standby_msg(state: &SharedState) -> Option<ControllerMsg> {
    let s = state.lock().unwrap();
    let (cue_id, layer) = s.run.standby.as_ref()?;
    let cue = s.show.as_ref()?.cues.iter().find(|c| &c.id == cue_id)?;
    Some(ControllerMsg::Standby(load_cue_for(cue, *layer)))
}

async fn handle_conn(stream: TcpStream, addr: SocketAddr, state: SharedState) -> Result<()> {
    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (mut sink, mut source) = ws.split();

    // Wait for HELLO.
    let hello = loop {
        match source.next().await {
            Some(Ok(WsMsg::Text(t))) => {
                let env: Envelope<ClientMsg> = serde_json::from_str(&t)?;
                if let ClientMsg::Hello(h) = env.msg {
                    break h;
                }
            }
            Some(Ok(WsMsg::Ping(p))) => {
                sink.send(WsMsg::Pong(p)).await?;
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(e.into()),
            None => return Ok(()),
        }
    };

    // Blacklist check. Compute the verdict and release the lock *before*
    // calling `log` (which re-locks `state`) — std Mutex is not reentrant,
    // so holding the guard across `log` would self-deadlock the task and
    // leave the state mutex poisoned-open, freezing the whole controller.
    let blacklisted = {
        let s = state.lock().unwrap();
        s.blacklist.iter().any(|id| id == &hello.client_id)
    };
    if blacklisted {
        log(&state, format!("rejecting blacklisted client {}", hello.client_id));
        return Ok(());
    }

    // Register the client.
    let (out_tx, mut out_rx) = mpsc::channel::<Outgoing>(OUTBOUND_QUEUE);
    let now_ms = now_utc_ms();
    let client_id = hello.client_id.clone();
    {
        let mut s = state.lock().unwrap();
        s.clients.insert(
            client_id.clone(),
            ClientRow {
                client_id: client_id.clone(),
                name: hello.name.clone(),
                addr: addr.to_string(),
                state: ClientState::Idle,
                current_cue: None,
                position_ms: 0,
                offset_ms: None,
                last_drift_ms: None,
                last_heartbeat_ms: now_ms,
                preflight: Default::default(),
                push_progress: None,
                outbound: out_tx.clone(),
            },
        );
        s.push_log(format!("client {} ({}) joined from {addr}", hello.name, client_id));
    }

    // Send HELLO_ACK directly, then the current show (if loaded) via the queue.
    let ack = Envelope::new(
        now_utc_ms(),
        ControllerMsg::HelloAck(HelloAck {
            controller_name: "cuemesh2-controller".into(),
            protocol_version: PROTOCOL_VERSION,
        }),
    );
    sink.send(WsMsg::Text(serde_json::to_string(&ack)?)).await?;
    if let Some(msg) = show_sync_msg(&state) {
        let _ = out_tx.try_send(Outgoing::Msg(msg));
    }
    // Catch a joining client up on the current standby so its GO is instant
    // too — the controller may have issued the standby before this client
    // (or any client) was connected.
    if let Some(msg) = standby_msg(&state) {
        let _ = out_tx.try_send(Outgoing::Msg(msg));
    }

    // Split loops: read → state, write ← channel.
    let state_reader = state.clone();
    let client_id_reader = client_id.clone();
    let reader = tokio::spawn(async move {
        while let Some(next) = source.next().await {
            let msg = match next {
                Ok(m) => m,
                Err(e) => {
                    log(&state_reader, format!("read error {client_id_reader}: {e}"));
                    break;
                }
            };
            match msg {
                WsMsg::Text(t) => {
                    let env: Envelope<ClientMsg> = match serde_json::from_str(&t) {
                        Ok(e) => e,
                        Err(e) => {
                            log(&state_reader, format!("bad json from {client_id_reader}: {e}"));
                            continue;
                        }
                    };
                    handle_client_msg(&state_reader, &client_id_reader, env);
                }
                WsMsg::Close(_) => break,
                _ => {}
            }
        }
    });

    let writer = tokio::spawn(async move {
        while let Some(out) = out_rx.recv().await {
            let ws_msg = match out {
                Outgoing::Msg(msg) => {
                    let env = Envelope::new(now_utc_ms(), msg);
                    match serde_json::to_string(&env) {
                        Ok(t) => WsMsg::Text(t),
                        Err(_) => continue,
                    }
                }
                Outgoing::Chunk(bytes) => WsMsg::Binary(bytes),
            };
            if sink.send(ws_msg).await.is_err() {
                break;
            }
        }
    });

    // The reader ends when the client disconnects. At that point deregister
    // the client (dropping the roster's clone of the outbound sender) and
    // drop our own clone, so the writer's channel closes and it can exit.
    // If we `join!`ed both instead, the writer would park forever on
    // `out_rx.recv()` because those two senders would still be alive.
    let _ = reader.await;
    {
        let mut s = state.lock().unwrap();
        s.clients.remove(&client_id);
        s.push_log(format!("client {client_id} left"));
    }
    drop(out_tx);
    let _ = writer.await;
    Ok(())
}

fn handle_client_msg(state: &SharedState, client_id: &str, env: Envelope<ClientMsg>) {
    match env.msg {
        ClientMsg::Status(s) => {
            let mut st = state.lock().unwrap();
            if let Some(row) = st.clients.get_mut(client_id) {
                row.state = s.state;
                row.current_cue = s.current_cue_id;
                row.position_ms = s.position_ms;
            }
        }
        ClientMsg::Drift(d) => {
            let mut st = state.lock().unwrap();
            if let Some(row) = st.clients.get_mut(client_id) {
                row.last_drift_ms = Some(d.drift_ms);
            }
        }
        ClientMsg::Heartbeat => {
            let now = now_utc_ms();
            let mut st = state.lock().unwrap();
            if let Some(row) = st.clients.get_mut(client_id) {
                row.last_heartbeat_ms = now;
            }
        }
        ClientMsg::SyncReply(reply) => {
            let t4 = now_utc_ms();
            let offset = cuemesh2_shared::clock_sync::compute_offset(
                reply.t1_utc_ms,
                reply.t2_local_ms,
                reply.t3_local_ms,
                t4,
            );
            let mut st = state.lock().unwrap();
            if let Some(row) = st.clients.get_mut(client_id) {
                row.offset_ms = Some(offset);
            }
        }
        ClientMsg::MediaReport(report) => {
            let mut st = state.lock().unwrap();
            let n_ok = report
                .entries
                .iter()
                .filter(|e| e.status == cuemesh2_shared::protocol::MediaFileStatus::Ok)
                .count();
            let total = report.entries.len();
            if let Some(row) = st.clients.get_mut(client_id) {
                row.preflight = report
                    .entries
                    .into_iter()
                    .map(|e| (e.rel_path, e.status))
                    .collect();
            }
            st.push_log(format!("preflight {client_id}: {n_ok}/{total} ok"));
        }
        ClientMsg::MediaPushProgress(p) => {
            let mut st = state.lock().unwrap();
            if let Some(row) = st.clients.get_mut(client_id) {
                if let Some((path, received, total)) = row.push_progress.as_mut() {
                    let _ = path;
                    *received = p.received_bytes;
                    *total = p.total_bytes;
                }
            }
        }
        ClientMsg::MediaPushResult(r) => {
            let mut st = state.lock().unwrap();
            if let Some(row) = st.clients.get_mut(client_id) {
                row.push_progress = None;
                if r.ok {
                    row.preflight
                        .insert(r.rel_path.clone(), cuemesh2_shared::protocol::MediaFileStatus::Ok);
                }
            }
            let verdict = if r.ok {
                "ok".to_string()
            } else {
                format!("FAILED: {}", r.error.unwrap_or_default())
            };
            st.push_log(format!("push {} → {client_id}: {verdict}", r.rel_path.display()));
        }
        ClientMsg::Log(l) => {
            state.lock().unwrap().push_log(format!(
                "[{}][{:?}] {}: {}",
                client_id, l.level, l.source, l.message
            ));
        }
        ClientMsg::Hello(_) | ClientMsg::Ready(_) => {
            // Ignored after initial handshake.
        }
    }
}

/// Enqueue a message to every connected client.
pub fn broadcast(state: &SharedState, msg: ControllerMsg) {
    let queues: Vec<_> = {
        let s = state.lock().unwrap();
        s.clients.values().map(|c| c.outbound.clone()).collect()
    };
    for q in queues {
        let _ = q.try_send(Outgoing::Msg(msg.clone()));
    }
}

/// Get one client's outbound queue.
pub fn client_queue(state: &SharedState, client_id: &str) -> Option<mpsc::Sender<Outgoing>> {
    state
        .lock()
        .unwrap()
        .clients
        .get(client_id)
        .map(|c| c.outbound.clone())
}

pub fn log(state: &SharedState, line: impl Into<String>) {
    let line = line.into();
    tracing::info!("{line}");
    state.lock().unwrap().push_log(line);
}

pub fn now_utc_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

//! Integration test: a fake client speaks the wire protocol to a real
//! controller server task — no GUI, no GStreamer.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as WsMsg;

use cuemesh2_controller::{server, state};
use cuemesh2_shared::protocol::{
    ClientMsg, ClientState, ControllerMsg, Envelope, Hello, MediaFileStatus, MediaReport,
    MediaReportEntry, Status, SyncReply, PROTOCOL_VERSION,
};
use cuemesh2_shared::show::ShowFile;

const TEST_SHOW: &str = r#"
[show]
title = "Handshake Test"
version = 1
media_root = "/tmp/nowhere"

[[cues]]
id = "c1"
name = "One"
type = "video"
file = "one.mp4"
fade_in_ms = 800
"#;

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

async fn recv_msg(
    ws: &mut (impl StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> ControllerMsg {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for controller message")
            .expect("stream ended")
            .expect("ws error");
        if let WsMsg::Text(t) = msg {
            let env: Envelope<ControllerMsg> = serde_json::from_str(&t).expect("bad json");
            return env.msg;
        }
    }
}

#[tokio::test]
async fn hello_gets_ack_show_sync_and_roster_entry() {
    let state = state::shared();
    state.lock().unwrap().show = Some(ShowFile::parse_str(TEST_SHOW).unwrap());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_state = state.clone();
    tokio::spawn(async move {
        let _ = server::serve(server_state, listener).await;
    });

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
        .await
        .expect("connect");

    // HELLO → HELLO_ACK + SHOW_SYNC (in order).
    let hello = Envelope::new(
        now_ms(),
        ClientMsg::Hello(Hello {
            client_id: "test-client-1".into(),
            name: "Test Client".into(),
            protocol_version: PROTOCOL_VERSION,
            app_version: "0.1.0".into(),
            target_triple: "x86_64-unknown-linux-gnu".into(),
        }),
    );
    ws.send(WsMsg::Text(serde_json::to_string(&hello).unwrap()))
        .await
        .unwrap();

    match recv_msg(&mut ws).await {
        ControllerMsg::HelloAck(a) => assert_eq!(a.protocol_version, PROTOCOL_VERSION),
        other => panic!("expected HELLO_ACK, got {other:?}"),
    }
    match recv_msg(&mut ws).await {
        ControllerMsg::ShowSync(s) => {
            assert_eq!(s.title, "Handshake Test");
            assert_eq!(s.cues.len(), 1);
            assert_eq!(s.cues[0].fade_in_ms, 800);
        }
        other => panic!("expected SHOW_SYNC, got {other:?}"),
    }

    // Status + sync reply + media report all land in the roster row.
    let status = Envelope::new(
        now_ms(),
        ClientMsg::Status(Status {
            state: ClientState::Playing,
            current_cue_id: Some("c1".into()),
            position_ms: 1234,
            rate: 1.0,
            layer_a_alpha: 1.0,
            layer_b_alpha: 0.0,
        }),
    );
    ws.send(WsMsg::Text(serde_json::to_string(&status).unwrap()))
        .await
        .unwrap();

    // Fabricate a client whose clock runs ~5s ahead of the controller. We
    // use a large skew (not a few ms) so the measured offset is dominated by
    // the fabricated clock difference and not by the test's own scheduling
    // latency — the NTP formula cannot separate "client +40ms" from
    // "client +0ms, 40ms processing delay" given statically-chosen stamps.
    const SKEW_MS: u64 = 5000;
    let t = now_ms();
    let send = now_ms();
    let reply = Envelope::new(
        t,
        ClientMsg::SyncReply(SyncReply {
            token: 1,
            t1_utc_ms: t,
            t2_local_ms: send + SKEW_MS,
            t3_local_ms: send + SKEW_MS,
        }),
    );
    ws.send(WsMsg::Text(serde_json::to_string(&reply).unwrap()))
        .await
        .unwrap();

    let report = Envelope::new(
        now_ms(),
        ClientMsg::MediaReport(MediaReport {
            entries: vec![MediaReportEntry {
                rel_path: "one.mp4".into(),
                status: MediaFileStatus::Missing,
            }],
        }),
    );
    ws.send(WsMsg::Text(serde_json::to_string(&report).unwrap()))
        .await
        .unwrap();

    // Give the reader task a moment to fold everything into state.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            {
                let s = state.lock().unwrap();
                if let Some(row) = s.clients.get("test-client-1") {
                    if let (ClientState::Playing, Some(off), false) =
                        (row.state, row.offset_ms, row.preflight.is_empty())
                    {
                        assert_eq!(row.name, "Test Client");
                        assert_eq!(row.current_cue.as_deref(), Some("c1"));
                        assert_eq!(row.position_ms, 1234);
                        // ~5s offset from the fabricated handshake, minus a
                        // few ms of round-trip. Wide lower bound tolerates a
                        // loaded CI machine.
                        assert!((4800..=5001).contains(&off), "offset {off} not ~5000");
                        assert_eq!(
                            row.preflight.get(std::path::Path::new("one.mp4")),
                            Some(&MediaFileStatus::Missing)
                        );
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("roster row never reached expected state");

    // Disconnect → row disappears.
    ws.close(None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if state.lock().unwrap().clients.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("client row was never removed after disconnect");
}

#[tokio::test]
async fn blacklisted_client_is_rejected() {
    let state = state::shared();
    state.lock().unwrap().blacklist.push("bad-client".into());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_state = state.clone();
    tokio::spawn(async move {
        let _ = server::serve(server_state, listener).await;
    });

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
        .await
        .expect("connect");
    let hello = Envelope::new(
        now_ms(),
        ClientMsg::Hello(Hello {
            client_id: "bad-client".into(),
            name: "Banned".into(),
            protocol_version: PROTOCOL_VERSION,
            app_version: "0.1.0".into(),
            target_triple: "x86_64-unknown-linux-gnu".into(),
        }),
    );
    ws.send(WsMsg::Text(serde_json::to_string(&hello).unwrap()))
        .await
        .unwrap();

    // Server closes without HELLO_ACK; next read is Close/None/error.
    let next = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("timed out");
    match next {
        None | Some(Ok(WsMsg::Close(_))) | Some(Err(_)) => {}
        Some(Ok(other)) => panic!("expected close, got {other:?}"),
    }
    assert!(state.lock().unwrap().clients.is_empty());
}

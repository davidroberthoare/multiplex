//! Integration test: the full controller-side update flow over a real
//! WebSocket — bundle load, per-client push (BEGIN/chunks/END framing),
//! roster bookkeeping from the client's result messages, and the
//! operator-apply message. A fake client speaks the wire protocol; no GUI,
//! no GStreamer.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as WsMsg;

use cuemesh2_controller::state::ClientUpdate;
use cuemesh2_controller::{server, state, update};
use cuemesh2_shared::protocol::{
    ClientMsg, ControllerMsg, Envelope, Hello, UpdatePushResult, PROTOCOL_VERSION,
};
use cuemesh2_shared::{hashing, transfer, update as shared_update};

/// Fixed test seed — NOT the release key.
const TEST_PRIV: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
const TEST_TRIPLE: &str = "x86_64-test-triple";

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

async fn recv_raw(
    ws: &mut (impl StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> WsMsg {
    tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("timed out waiting for controller message")
        .expect("stream ended")
        .expect("ws error")
}

async fn recv_msg(
    ws: &mut (impl StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> ControllerMsg {
    loop {
        if let WsMsg::Text(t) = recv_raw(ws).await {
            let env: Envelope<ControllerMsg> = serde_json::from_str(&t).expect("bad json");
            return env.msg;
        }
    }
}

#[tokio::test]
async fn push_update_streams_verified_binary_and_apply_follows() {
    // ── Signed test bundle in a temp dir ──────────────────────────────────
    let bundle = std::env::temp_dir().join(format!("cuemesh2_update_push_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&bundle);
    std::fs::create_dir_all(&bundle).unwrap();

    // A "binary" spanning several chunks so framing/reassembly is exercised.
    let binary: Vec<u8> = (0..transfer::CHUNK_SIZE * 2 + 12345)
        .map(|i| (i % 251) as u8)
        .collect();
    let sha256 = hashing::to_hex(&hashing::sha256_bytes(&binary));
    let signature = shared_update::sign_detached(TEST_PRIV, &binary).unwrap();
    let pubkey = shared_update::pubkey_of(TEST_PRIV).unwrap();
    std::fs::write(bundle.join("fake-client"), &binary).unwrap();
    std::fs::write(
        bundle.join("manifest.toml"),
        format!(
            "version = \"99.0.0\"\n\n[clients.{TEST_TRIPLE}]\nfile = \"fake-client\"\nsha256 = \"{sha256}\"\nsignature = \"{signature}\"\nmin_gstreamer = \"1.18\"\n"
        ),
    )
    .unwrap();

    // Process-global env: this file has exactly one test, so no races.
    std::env::set_var("CUEMESH_UPDATE_BUNDLE", &bundle);
    std::env::set_var("CUEMESH_UPDATE_PUBKEY", &pubkey);

    // ── Controller with the bundle loaded ─────────────────────────────────
    let state = state::shared();
    update::load_local_manifest(&state);
    assert_eq!(
        state
            .lock()
            .unwrap()
            .update_manifest
            .as_ref()
            .map(|m| m.version.clone()),
        Some("99.0.0".to_string()),
        "bundle manifest should load"
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_state = state.clone();
    tokio::spawn(async move {
        let _ = server::serve(server_state, listener).await;
    });

    // ── Fake out-of-date client joins ─────────────────────────────────────
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
        .await
        .expect("connect");
    let hello = Envelope::new(
        now_ms(),
        ClientMsg::Hello(Hello {
            client_id: "updatee".into(),
            name: "Updatee".into(),
            protocol_version: PROTOCOL_VERSION,
            app_version: "0.1.0".into(),
            target_triple: TEST_TRIPLE.into(),
        }),
    );
    ws.send(WsMsg::Text(serde_json::to_string(&hello).unwrap()))
        .await
        .unwrap();
    match recv_msg(&mut ws).await {
        ControllerMsg::HelloAck(_) => {}
        other => panic!("expected HELLO_ACK, got {other:?}"),
    }

    // ── Operator pushes the update ────────────────────────────────────────
    update::push_update_to(&state, "updatee".into());

    let begin = match recv_msg(&mut ws).await {
        ControllerMsg::UpdatePushBegin(b) => b,
        other => panic!("expected UPDATE_PUSH_BEGIN, got {other:?}"),
    };
    assert_eq!(begin.version, "99.0.0");
    assert_eq!(begin.target_triple, TEST_TRIPLE);
    assert_eq!(begin.size, binary.len() as u64);
    assert_eq!(begin.sha256_hex, sha256);
    assert_eq!(begin.min_gstreamer.as_deref(), Some("1.18"));

    // Reassemble the chunk stream up to UPDATE_PUSH_END.
    let mut received = Vec::with_capacity(binary.len());
    loop {
        match recv_raw(&mut ws).await {
            WsMsg::Binary(frame) => {
                let (id, data) = transfer::decode_chunk(&frame).expect("bad frame");
                assert_eq!(id, begin.transfer_id);
                received.extend_from_slice(data);
            }
            WsMsg::Text(t) => {
                let env: Envelope<ControllerMsg> = serde_json::from_str(&t).unwrap();
                match env.msg {
                    ControllerMsg::UpdatePushEnd(e) => {
                        assert_eq!(e.transfer_id, begin.transfer_id);
                        break;
                    }
                    other => panic!("expected chunks then UPDATE_PUSH_END, got {other:?}"),
                }
            }
            other => panic!("unexpected frame {other:?}"),
        }
    }

    // What arrived is byte-identical and carries a valid release signature —
    // exactly what the real client verifies before staging.
    assert_eq!(received, binary);
    shared_update::verify_signature(&pubkey, &received, &begin.signature_b64)
        .expect("signature must verify");

    // ── Client reports staged; roster follows ─────────────────────────────
    let result = Envelope::new(
        now_ms(),
        ClientMsg::UpdatePushResult(UpdatePushResult {
            transfer_id: begin.transfer_id,
            version: begin.version.clone(),
            ok: true,
            error: None,
        }),
    );
    ws.send(WsMsg::Text(serde_json::to_string(&result).unwrap()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            {
                let s = state.lock().unwrap();
                if let Some(row) = s.clients.get("updatee") {
                    if row.update == ClientUpdate::Staged("99.0.0".into()) {
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("roster should mark the client staged");

    // ── Operator applies ──────────────────────────────────────────────────
    update::send_apply(&state, "updatee");
    match recv_msg(&mut ws).await {
        ControllerMsg::ApplyUpdate => {}
        other => panic!("expected APPLY_UPDATE, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&bundle);
}

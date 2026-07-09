//! Full-loop client test: a fake controller pushes a signed update over a
//! real WebSocket to the real client connection task (real MediaEngine, no
//! window). Covers the receive path end to end — transfer routing, staging,
//! verification, the result message — plus the busy-refusal of APPLY_UPDATE.
//!
//! The happy-path APPLY (self-replace + re-exec) is deliberately not driven:
//! it would replace and restart the test binary itself. `self-replace` is a
//! dedicated, widely used crate; everything up to that call is exercised.
//!
//! One test function on purpose: the staged slot and the pubkey env var are
//! process-global.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as WsMsg;

use cuemesh2_client::state::PlaybackState;
use cuemesh2_client::update::{discard_staged, staged_bin_path, staged_version};
use cuemesh2_client::{connection, state};
use cuemesh2_media::MediaEngine;
use cuemesh2_shared::protocol::{
    ClientMsg, ControllerMsg, Envelope, UpdatePushBegin, UpdatePushEnd,
};
use cuemesh2_shared::{hashing, transfer, update as shared_update};

/// Fixed test seed — NOT the release key.
const TEST_PRIV: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[tokio::test(flavor = "multi_thread")]
async fn client_stages_pushed_update_and_refuses_apply_while_playing() {
    std::env::set_var(
        "CUEMESH_UPDATE_PUBKEY",
        shared_update::pubkey_of(TEST_PRIV).unwrap(),
    );
    discard_staged();

    // ── Fake controller ───────────────────────────────────────────────────
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // ── Real client connection task ───────────────────────────────────────
    let engine = MediaEngine::new().expect("GStreamer runtime required for this test");
    let client_state = state::shared();
    let media_root = std::env::temp_dir().join("cuemesh2_update_over_wire_media");
    let _ = std::fs::create_dir_all(&media_root);
    {
        let run_state = client_state.clone();
        let run_engine = engine.clone();
        let cfg = connection::ConnectionConfig {
            controller_url: format!("ws://{addr}"),
            client_id: "wire-test".into(),
            name: "Wire Test".into(),
            media_root,
        };
        tokio::spawn(async move {
            connection::run(cfg, run_state, run_engine).await;
        });
    }

    let (stream, _) = listener.accept().await.unwrap();
    let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
    let (mut sink, mut source) = ws.split();

    // Wait for HELLO and check the new fields ride along.
    let hello = wait_for(&mut source, |msg| match msg {
        ClientMsg::Hello(h) => Some(h),
        _ => None,
    })
    .await;
    assert_eq!(hello.app_version, env!("CARGO_PKG_VERSION"));
    assert!(!hello.target_triple.is_empty());

    // ── Push a signed update ──────────────────────────────────────────────
    let binary: Vec<u8> = (0..transfer::CHUNK_SIZE + 999)
        .map(|i| (i % 249) as u8)
        .collect();
    let transfer_id = 424242u64;
    let begin = ControllerMsg::UpdatePushBegin(UpdatePushBegin {
        transfer_id,
        target_triple: hello.target_triple.clone(),
        version: "99.0.0".into(),
        size: binary.len() as u64,
        sha256_hex: hashing::to_hex(&hashing::sha256_bytes(&binary)),
        signature_b64: shared_update::sign_detached(TEST_PRIV, &binary).unwrap(),
        min_gstreamer: Some("1.18".into()),
    });
    send(&mut sink, begin).await;
    for chunk in binary.chunks(transfer::CHUNK_SIZE) {
        sink.send(WsMsg::Binary(transfer::encode_chunk(transfer_id, chunk)))
            .await
            .unwrap();
    }
    send(
        &mut sink,
        ControllerMsg::UpdatePushEnd(UpdatePushEnd { transfer_id }),
    )
    .await;

    let result = wait_for(&mut source, |msg| match msg {
        ClientMsg::UpdatePushResult(r) => Some(r),
        _ => None,
    })
    .await;
    assert!(result.ok, "stage should succeed: {:?}", result.error);
    assert_eq!(result.version, "99.0.0");
    assert_eq!(staged_version().as_deref(), Some("99.0.0"));
    assert_eq!(
        std::fs::read(staged_bin_path().unwrap()).unwrap(),
        binary,
        "staged binary must be byte-identical"
    );

    // ── APPLY_UPDATE while playing must be refused ────────────────────────
    client_state.lock().unwrap().playback.state = PlaybackState::Playing;
    send(&mut sink, ControllerMsg::ApplyUpdate).await;
    let refusal = wait_for(&mut source, |msg| match msg {
        ClientMsg::UpdateApplyResult(r) => Some(r),
        _ => None,
    })
    .await;
    assert!(!refusal.ok);
    assert!(
        refusal
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("busy"),
        "refusal should say busy: {:?}",
        refusal.error
    );
    // The staged update survives a refused apply, ready for later.
    assert_eq!(staged_version().as_deref(), Some("99.0.0"));

    discard_staged();
}

async fn send(
    sink: &mut (impl SinkExt<WsMsg, Error = tokio_tungstenite::tungstenite::Error> + Unpin),
    msg: ControllerMsg,
) {
    let env = Envelope::new(now_ms(), msg);
    sink.send(WsMsg::Text(serde_json::to_string(&env).unwrap()))
        .await
        .unwrap();
}

/// Read client messages (skipping status/heartbeat noise) until `pick`
/// matches, with a timeout.
async fn wait_for<T>(
    source: &mut (impl StreamExt<Item = Result<WsMsg, tokio_tungstenite::tungstenite::Error>> + Unpin),
    pick: impl Fn(ClientMsg) -> Option<T>,
) -> T {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let Some(Ok(msg)) = source.next().await else {
                panic!("client stream ended");
            };
            if let WsMsg::Text(t) = msg {
                let env: Envelope<ClientMsg> = serde_json::from_str(&t).expect("bad json");
                if let Some(v) = pick(env.msg) {
                    return v;
                }
            }
        }
    })
    .await
    .expect("timed out waiting for client message")
}

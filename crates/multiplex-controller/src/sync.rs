//! Periodic SYNC ping loop.
//!
//! Pings go per-client (not broadcast) so each ping can carry the
//! controller's last offset measurement for that specific client — the
//! client medians those to convert master timestamps into local time.

use std::time::Duration;

use cuemesh2_shared::protocol::{ControllerMsg, SyncPing};

use crate::server::now_utc_ms;
use crate::state::{Outgoing, SharedState};

pub async fn run(state: SharedState) {
    let mut interval = tokio::time::interval(Duration::from_millis(1000));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut token: u64 = 0;
    loop {
        interval.tick().await;
        token = token.wrapping_add(1);
        let targets: Vec<_> = {
            let s = state.lock().unwrap();
            s.clients
                .values()
                .map(|c| (c.outbound.clone(), c.offset_ms))
                .collect()
        };
        for (queue, offset) in targets {
            let _ = queue.try_send(Outgoing::Msg(ControllerMsg::Sync(SyncPing {
                t1_utc_ms: now_utc_ms(),
                token,
                last_offset_ms: offset,
            })));
        }
    }
}

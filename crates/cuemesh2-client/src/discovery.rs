//! mDNS browsing: find controllers on the LAN and surface them in the
//! connect UI. Manual URL entry always remains available as the fallback.

use std::collections::HashMap;

use mdns_sd::{ServiceDaemon, ServiceEvent};

use cuemesh2_shared::protocol::MDNS_SERVICE_TYPE;

use crate::state::SharedState;

/// Browse for controllers forever, mirroring findings into
/// `state.discovered` (instance name → ws URL). Never fatal.
pub fn spawn_browser(state: SharedState) {
    std::thread::Builder::new()
        .name("cuemesh2-mdns".into())
        .spawn(move || {
            let daemon = match ServiceDaemon::new() {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(%e, "mDNS browse unavailable");
                    return;
                }
            };
            let receiver = match daemon.browse(MDNS_SERVICE_TYPE) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(%e, "mDNS browse failed");
                    return;
                }
            };
            // Track resolved instances so removals only delete what we added.
            let mut known: HashMap<String, String> = HashMap::new();
            while let Ok(event) = receiver.recv() {
                match event {
                    ServiceEvent::ServiceResolved(info) => {
                        let Some(addr) = info.get_addresses().iter().next().copied() else {
                            continue;
                        };
                        let url = format!("ws://{}:{}", addr, info.get_port());
                        let name = info.get_fullname().to_string();
                        tracing::info!(%name, %url, "controller discovered");
                        known.insert(name.clone(), url.clone());
                        state.lock().unwrap().discovered.insert(name, url);
                    }
                    ServiceEvent::ServiceRemoved(_ty, fullname) if known.remove(&fullname).is_some() => {
                        state.lock().unwrap().discovered.remove(&fullname);
                    }
                    _ => {}
                }
            }
        })
        .expect("spawn mdns browser thread");
}

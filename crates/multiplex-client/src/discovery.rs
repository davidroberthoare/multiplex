//! mDNS browsing: find controllers on the LAN and surface them in the
//! connect UI. Manual URL entry always remains available as the fallback.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

use cuemesh2_shared::protocol::MDNS_SERVICE_TYPE;

use crate::state::SharedState;

/// Pick which of a resolved service's addresses to connect to, preferring
/// IPv4. `get_addresses()` is a `HashSet`, whose iteration order is
/// randomized per-process, so picking the first entry directly flips
/// between a routable IPv4 address and an IPv6 one from run to run.
/// Link-local IPv6 (fe80::/10, what mDNS mostly advertises) needs an
/// interface scope id (`%eth0`) to be reachable at all, which a bare
/// `IpAddr` can't carry — not worth chasing on an offline, consumer-router
/// LAN tool — so it's only used as a fallback when no v4 address exists.
fn pick_address(addrs: &HashSet<IpAddr>) -> Option<IpAddr> {
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or_else(|| addrs.iter().next())
        .copied()
}

/// Build a `ws://` URL for an address, bracketing IPv6 — a bare IPv6
/// address in a "host:port" position is ambiguous with the port separator.
fn ws_url(addr: IpAddr, port: u16) -> String {
    match addr {
        IpAddr::V4(v4) => format!("ws://{v4}:{port}"),
        IpAddr::V6(v6) => format!("ws://[{v6}]:{port}"),
    }
}

fn resolved_url(info: &ServiceInfo) -> Option<String> {
    pick_address(info.get_addresses()).map(|addr| ws_url(addr, info.get_port()))
}

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
                        let Some(url) = resolved_url(&info) else {
                            continue;
                        };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_ipv4_when_both_present() {
        let addrs: HashSet<IpAddr> = [
            "fe80::1".parse().unwrap(),
            "192.168.1.50".parse().unwrap(),
        ]
        .into_iter()
        .collect();
        assert_eq!(pick_address(&addrs), Some("192.168.1.50".parse().unwrap()));
    }

    #[test]
    fn falls_back_to_ipv6_when_only_option() {
        let addrs: HashSet<IpAddr> = ["fe80::1".parse().unwrap()].into_iter().collect();
        assert_eq!(pick_address(&addrs), Some("fe80::1".parse().unwrap()));
    }

    #[test]
    fn empty_address_set_yields_none() {
        assert_eq!(pick_address(&HashSet::new()), None);
    }

    #[test]
    fn ipv4_url_is_unbracketed() {
        assert_eq!(ws_url("192.168.1.50".parse().unwrap(), 9420), "ws://192.168.1.50:9420");
    }

    #[test]
    fn ipv6_url_is_bracketed() {
        assert_eq!(ws_url("fe80::1".parse().unwrap(), 9420), "ws://[fe80::1]:9420");
    }
}

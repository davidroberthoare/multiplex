//! mDNS advertisement: lets clients on the LAN find this controller without
//! typing an IP. Advertises `_cuemesh._tcp.local.` with our WebSocket port.

use mdns_sd::{ServiceDaemon, ServiceInfo};

use cuemesh2_shared::protocol::MDNS_SERVICE_TYPE;

/// Register the service and keep the daemon alive for the process lifetime.
/// Failure is logged, never fatal — manual IP entry always works.
pub fn advertise(port: u16) {
    let hostname = hostname();
    let instance = format!("CueMesh2 Controller ({hostname})");
    match try_advertise(&instance, &hostname, port) {
        Ok(daemon) => {
            tracing::info!(%instance, port, "mDNS advertisement registered");
            // Leak the daemon: it must outlive this function, and there is
            // exactly one per process.
            std::mem::forget(daemon);
        }
        Err(e) => {
            tracing::warn!(%e, "mDNS advertisement failed; clients need manual IP");
        }
    }
}

fn try_advertise(
    instance: &str,
    hostname: &str,
    port: u16,
) -> Result<ServiceDaemon, mdns_sd::Error> {
    let daemon = ServiceDaemon::new()?;
    let service = ServiceInfo::new(
        MDNS_SERVICE_TYPE,
        instance,
        &format!("{hostname}.local."),
        // Empty address: mdns-sd auto-detects interface addresses.
        "",
        port,
        None,
    )?
    .enable_addr_auto();
    daemon.register(service)?;
    Ok(daemon)
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|h| !h.is_empty())
        })
        .unwrap_or_else(|| "cuemesh2".into())
}

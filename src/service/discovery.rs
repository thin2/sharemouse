//! Local-network discovery of devices that are in joining mode.
//!
//! Beacons are unauthenticated hints that only fill in the main device
//! address. Pairing still requires both users to compare and approve the
//! certificate fingerprints, so a spoofed beacon cannot grant any access.
use super::*;
use lan_mouse_ipc::DiscoveredHost;
use serde::{Deserialize, Serialize};
use std::{
    collections::hash_map::RandomState,
    hash::{BuildHasher, Hasher},
    net::{Ipv4Addr, SocketAddrV4},
    time::{Duration, Instant},
};
use tokio::{net::UdpSocket, sync::mpsc};

pub(super) const PORT: u16 = 4245;
pub(super) const INTERVAL: Duration = Duration::from_secs(1);
const MAGIC: &str = "sharemouse-join-v1";
const EXPIRY: Duration = Duration::from_secs(5);
const MAX_HOSTS: usize = 16;
const MAX_NAME_CHARS: usize = 64;
const MAX_PACKET: usize = 512;

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Beacon {
    magic: String,
    /// Random per-process value so a device ignores its own beacons.
    instance: u64,
    name: String,
    port: u16,
}

pub(super) struct Found {
    ip: Ipv4Addr,
    port: u16,
    name: String,
}

pub(super) struct Discovery {
    instance: u64,
    name: String,
    pub events: mpsc::Receiver<Found>,
    // Kept so `events` stays pending instead of closing when no listener runs.
    _tx: mpsc::Sender<Found>,
    hosts: HashMap<SocketAddrV4, (String, Instant)>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Discovery {
    pub async fn start() -> Self {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u32(std::process::id());
        let instance = hasher.finish();
        let name = hostname::get()
            .ok()
            .and_then(|name| name.into_string().ok())
            .map(|name| clean_name(&name))
            .unwrap_or_else(|| clean_name(""));
        let (tx, events) = mpsc::channel(32);
        let task = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, PORT)).await {
            Ok(socket) => Some(tokio::task::spawn_local(receive(socket, instance, tx.clone()))),
            Err(e) => {
                log::warn!("nearby main devices unavailable, UDP {PORT}: {e}");
                None
            }
        };
        Self {
            instance,
            name,
            events,
            _tx: tx,
            hosts: HashMap::new(),
            task,
        }
    }

    pub fn beacons(&self, ips: &[Ipv4Addr], port: u16) -> Beacons {
        let payload = serde_json::to_vec(&Beacon {
            magic: MAGIC.into(),
            instance: self.instance,
            name: self.name.clone(),
            port,
        })
        .expect("beacon serializes");
        let sockets = ips
            .iter()
            .filter_map(|ip| match bind_broadcast(*ip) {
                Ok(socket) => Some((*ip, socket)),
                Err(e) => {
                    log::debug!("cannot announce joining mode on {ip}: {e}");
                    None
                }
            })
            .collect();
        Beacons { sockets, payload }
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

fn bind_broadcast(ip: Ipv4Addr) -> io::Result<UdpSocket> {
    let socket = std::net::UdpSocket::bind((ip, 0))?;
    socket.set_broadcast(true)?;
    socket.set_nonblocking(true)?;
    UdpSocket::from_std(socket)
}

/// Joining-mode announcements, one socket per local IPv4 so each adapter
/// (including a Windows hotspot) is announced on its own network.
pub(super) struct Beacons {
    sockets: Vec<(Ipv4Addr, UdpSocket)>,
    payload: Vec<u8>,
}

impl Beacons {
    pub async fn send(&self) {
        for (ip, socket) in &self.sockets {
            for target in broadcast_targets(*ip) {
                if let Err(e) = socket.send_to(&self.payload, (target, PORT)).await {
                    log::debug!("joining-mode announcement from {ip} to {target} failed: {e}");
                }
            }
        }
    }
}

/// Limited broadcast is not forwarded on every OS for non-primary adapters,
/// so also try the directed broadcast of the common /24 home-network prefix.
fn broadcast_targets(ip: Ipv4Addr) -> Vec<Ipv4Addr> {
    let [a, b, c, _] = ip.octets();
    let directed = Ipv4Addr::new(a, b, c, 255);
    if directed == ip {
        vec![Ipv4Addr::BROADCAST]
    } else {
        vec![Ipv4Addr::BROADCAST, directed]
    }
}

async fn receive(socket: UdpSocket, instance: u64, tx: mpsc::Sender<Found>) {
    let mut buf = [0u8; MAX_PACKET];
    loop {
        let (len, from) = match socket.recv_from(&mut buf).await {
            Ok(received) => received,
            Err(e) => {
                // Windows reports ICMP port-unreachable as a receive error.
                log::debug!("discovery receive failed: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        let SocketAddr::V4(from) = from else { continue };
        if let Some(found) = parse_beacon(&buf[..len], *from.ip(), instance) {
            let _ = tx.try_send(found);
        }
    }
}

fn parse_beacon(data: &[u8], ip: Ipv4Addr, instance: u64) -> Option<Found> {
    let beacon: Beacon = serde_json::from_slice(data).ok()?;
    if beacon.magic != MAGIC
        || beacon.instance == instance
        || beacon.port == 0
        || ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || ip.is_broadcast()
    {
        return None;
    }
    Some(Found {
        ip,
        port: beacon.port,
        name: clean_name(&beacon.name),
    })
}

fn clean_name(name: &str) -> String {
    let name: String = name
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME_CHARS)
        .collect();
    let name = name.trim();
    if name.is_empty() {
        "Sharemouse".into()
    } else {
        name.into()
    }
}

impl Service {
    pub(super) fn handle_discovered(&mut self, found: Found) {
        let key = SocketAddrV4::new(found.ip, found.port);
        let hosts = &mut self.discovery.hosts;
        if !hosts.contains_key(&key) && hosts.len() >= MAX_HOSTS {
            return;
        }
        let changed = hosts
            .insert(key, (found.name.clone(), Instant::now()))
            .is_none_or(|(name, _)| name != found.name);
        if changed {
            self.publish_discovered_hosts();
        }
    }

    pub(super) fn expire_discovered_hosts(&mut self) {
        let before = self.discovery.hosts.len();
        self.discovery.hosts.retain(|_, (_, seen)| seen.elapsed() < EXPIRY);
        if self.discovery.hosts.len() != before {
            self.publish_discovered_hosts();
        }
    }

    pub(super) fn publish_discovered_hosts(&mut self) {
        let mut hosts: Vec<_> = self
            .discovery
            .hosts
            .iter()
            .map(|(addr, (name, _))| (name.clone(), *addr))
            .collect();
        hosts.sort();
        let hosts = hosts
            .into_iter()
            .map(|(name, addr)| DiscoveredHost {
                name,
                address: enrollment::format_address(*addr.ip(), addr.port()),
            })
            .collect();
        self.notify_frontend(FrontendEvent::DiscoveredHosts(hosts));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(instance: u64, name: &str, port: u16) -> Vec<u8> {
        serde_json::to_vec(&Beacon {
            magic: MAGIC.into(),
            instance,
            name: name.into(),
            port,
        })
        .unwrap()
    }

    #[test]
    fn beacon_uses_sender_address_and_ignores_own_or_invalid_packets() {
        let ip = Ipv4Addr::new(192, 0, 2, 10);
        let found = parse_beacon(&packet(1, "Desk\u{7}top ", 4244), ip, 2).unwrap();
        assert_eq!((found.ip, found.port, found.name.as_str()), (ip, 4244, "Desktop"));
        assert!(parse_beacon(&packet(2, "Self", 4244), ip, 2).is_none());
        assert!(parse_beacon(&packet(1, "Zero", 0), ip, 2).is_none());
        assert!(parse_beacon(&packet(1, "Loop", 4244), Ipv4Addr::LOCALHOST, 2).is_none());
        assert!(parse_beacon(b"not json", ip, 2).is_none());
        let mut other = Beacon {
            magic: "other-app".into(),
            instance: 1,
            name: "x".into(),
            port: 4244,
        };
        assert!(parse_beacon(&serde_json::to_vec(&other).unwrap(), ip, 2).is_none());
        other.magic = MAGIC.into();
        assert!(parse_beacon(&serde_json::to_vec(&other).unwrap(), ip, 2).is_some());
    }

    #[test]
    fn names_are_bounded_and_never_empty() {
        assert_eq!(clean_name("  \n "), "Sharemouse");
        assert_eq!(clean_name(&"名".repeat(100)).chars().count(), MAX_NAME_CHARS);
        assert!(packet(1, &clean_name(&"x".repeat(1000)), 65535).len() <= MAX_PACKET);
    }

    #[test]
    fn broadcast_targets_include_directed_subnet() {
        assert_eq!(
            broadcast_targets(Ipv4Addr::new(192, 168, 137, 1)),
            [Ipv4Addr::BROADCAST, Ipv4Addr::new(192, 168, 137, 255)]
        );
        assert_eq!(
            broadcast_targets(Ipv4Addr::new(10, 0, 0, 255)),
            [Ipv4Addr::BROADCAST]
        );
    }
}

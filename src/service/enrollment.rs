use super::*;
use lan_mouse_data::enrollment::{self as wire, Connection, Group, Packet, Peer};
use std::{
    net::Ipv4Addr,
    sync::atomic::{AtomicU64, Ordering},
};
use tokio::sync::{mpsc, oneshot};

static NEXT_APPROVAL: AtomicU64 = AtomicU64::new(1);
pub(super) enum Event {
    Approval {
        epoch: u64,
        id: u64,
        peer: Peer,
        decision: oneshot::Sender<bool>,
    },
    Accepted {
        epoch: u64,
        peer: Peer,
        local: Peer,
        reply: oneshot::Sender<Result<Group, String>>,
    },
    Joined {
        epoch: u64,
        group: Group,
    },
    Failed {
        epoch: u64,
        reason: String,
    },
}
pub(super) struct Enrollment {
    identity: lan_mouse_data::Identity,
    pub group: Option<Group>,
    pub events: mpsc::Receiver<Event>,
    tx: mpsc::Sender<Event>,
    pub pending: HashMap<u64, oneshot::Sender<bool>>,
    task: Option<tokio::task::JoinHandle<()>>,
    epoch: u64,
    addresses: Vec<String>,
    pub status: String,
    ports: tokio::sync::watch::Sender<(u16, u16)>,
}
impl Enrollment {
    pub fn new(identity: lan_mouse_data::Identity, group: Option<Group>) -> Self {
        let (tx, events) = mpsc::channel(16);
        let group = group.filter(Group::valid);
        let status = if group.is_some() { "joined" } else { "idle" }.into();
        Self {
            identity,
            group,
            events,
            tx,
            pending: HashMap::new(),
            task: None,
            epoch: 0,
            addresses: vec![],
            status,
            ports: tokio::sync::watch::channel((0, 0)).0,
        }
    }
    pub fn stop(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.pending.clear();
        self.addresses.clear();
    }
}
impl Drop for Enrollment {
    fn drop(&mut self) {
        self.stop();
    }
}

fn parse_address(value: &str) -> Result<SocketAddr, String> {
    let trimmed = value.trim();
    let addr = trimmed
        .parse::<Ipv4Addr>()
        .map(|ip| SocketAddr::from((ip, wire::PORT)))
        .or_else(|_| trimmed.parse::<SocketAddr>())
        .map_err(|_| "Enter the main device's IPv4 address, for example 192.168.1.10".to_owned())?;
    if !matches!(addr.ip(), IpAddr::V4(ip) if !ip.is_unspecified() && !ip.is_multicast() && !ip.is_broadcast())
        || addr.port() == 0
    {
        return Err("Invalid main device IPv4 address or port".into());
    }
    Ok(addr)
}

fn local_ip() -> Option<Ipv4Addr> {
    // UDP connect selects a local route but sends no packet.
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) => Some(ip),
        _ => None,
    }
}

pub(super) fn format_address(ip: Ipv4Addr, port: u16) -> String {
    if port == wire::PORT {
        ip.to_string()
    } else {
        format!("{ip}:{port}")
    }
}

fn usable_ipv4(addresses: impl IntoIterator<Item = IpAddr>) -> Vec<Ipv4Addr> {
    let mut addresses: Vec<_> = addresses
        .into_iter()
        .filter_map(|ip| match ip {
            IpAddr::V4(ip)
                if !ip.is_unspecified()
                    && !ip.is_loopback()
                    && !ip.is_link_local()
                    && !ip.is_multicast()
                    && !ip.is_broadcast() =>
            {
                Some(ip)
            }
            _ => None,
        })
        .collect();
    addresses.sort_unstable();
    addresses.dedup();
    addresses
}

#[cfg(test)]
fn joining_addresses(addresses: impl IntoIterator<Item = IpAddr>, port: u16) -> Vec<String> {
    usable_ipv4(addresses)
        .into_iter()
        .map(|ip| format_address(ip, port))
        .collect()
}

fn local_ipv4() -> Vec<Ipv4Addr> {
    let mut addresses = match webrtc_util::ifaces::ifaces() {
        Ok(interfaces) => interfaces
            .into_iter()
            .filter_map(|interface| interface.addr.map(|addr| addr.ip()))
            .collect::<Vec<_>>(),
        Err(error) => {
            log::warn!("Could not enumerate joining addresses: {error}");
            Vec::new()
        }
    };
    if let Some(ip) = local_ip() {
        addresses.push(IpAddr::V4(ip));
    }
    usable_ipv4(addresses)
}

async fn pair(
    mut connection: Connection,
    host: bool,
    identity: lan_mouse_data::Identity,
    ports: (u16, u16),
    tx: mpsc::Sender<Event>,
    epoch: u64,
) -> Result<(), String> {
    let peer = connection
        .hello(ports.0, ports.1)
        .await
        .map_err(|e| e.to_string())?;
    if peer.fingerprint == identity.fingerprint() {
        return Err("Cannot join this device itself".into());
    }
    let id = NEXT_APPROVAL.fetch_add(1, Ordering::Relaxed);
    let (decision, answer) = oneshot::channel();
    tx.send(Event::Approval {
        epoch,
        id,
        peer: peer.clone(),
        decision,
    })
    .await
    .map_err(|e| e.to_string())?;
    let approve = answer.await.unwrap_or(false);
    connection
        .confirm(approve)
        .await
        .map_err(|e| e.to_string())?;
    if host {
        let IpAddr::V4(ip) = connection.local.ip() else {
            return Err("IPv4 required".into());
        };
        let local = Peer {
            fingerprint: identity.fingerprint(),
            ip,
            input_port: ports.0,
            data_port: ports.1,
        };
        let (reply, group) = oneshot::channel();
        tx.send(Event::Accepted {
            epoch,
            peer,
            local,
            reply,
        })
        .await
        .map_err(|e| e.to_string())?;
        match group.await.map_err(|e| e.to_string())? {
            Ok(group) => connection
                .send(&Packet::Group(group))
                .await
                .map_err(|e| e.to_string())?,
            Err(reason) => {
                let _ = connection.send(&Packet::Error(reason.clone())).await;
                return Err(reason);
            }
        }
    } else {
        let group = match connection.recv().await.map_err(|e| e.to_string())? {
            Packet::Group(group)
                if group.valid()
                    && group.leader == peer.fingerprint
                    && group.peers.iter().any(|p| p == &peer)
                    && group.peers.iter().any(|p| {
                        p.fingerprint == identity.fingerprint()
                            && IpAddr::V4(p.ip) == connection.local.ip()
                            && p.input_port == ports.0
                            && p.data_port == ports.1
                    }) =>
            {
                group
            }
            Packet::Error(reason) => return Err(reason),
            _ => return Err("Invalid connection group".into()),
        };
        tx.send(Event::Joined { epoch, group })
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

async fn bounded_pair(
    connection: io::Result<Connection>,
    host: bool,
    identity: lan_mouse_data::Identity,
    ports: (u16, u16),
    tx: mpsc::Sender<Event>,
    epoch: u64,
) {
    let result = match connection {
        Ok(connection) => tokio::time::timeout(
            wire::TIMEOUT,
            pair(connection, host, identity, ports, tx.clone(), epoch),
        )
        .await
        .map_err(|_| "Pairing confirmation timed out".to_owned())
        .and_then(|r| r),
        Err(e) => Err(e.to_string()),
    };
    if let Err(reason) = result {
        let _ = tx.send(Event::Failed { epoch, reason }).await;
    }
}

impl Service {
    pub(super) fn restore_connection_group(&mut self) {
        self.enrollment.stop();
        self.enrollment.group = self.config.connection_group().filter(|group| {
            group.valid()
                && group.peers.iter().all(|peer| {
                    peer.fingerprint == self.public_key_fingerprint
                        || self
                            .allowed_fingerprints
                            .read()
                            .expect("lock")
                            .contains(&peer.fingerprint)
                })
        });
        if let Some(group) = self.enrollment.group.clone() {
            self.install_connection_group(group);
            self.update_enrollment_port();
            self.publish_enrollment_state("joined");
        } else {
            self.config.clear_connection_group();
            self.publish_enrollment_state("idle");
        }
    }

    pub(super) fn publish_enrollment_state(&mut self, status: &str) {
        self.enrollment.status = status.into();
        self.notify_frontend(FrontendEvent::EnrollmentState {
            addresses: self.enrollment.addresses.clone(),
            status: status.into(),
        });
    }
    pub(super) async fn host_connection_group(&mut self, port: Option<u16>) {
        if self
            .enrollment
            .group
            .as_ref()
            .is_some_and(|g| g.leader != self.public_key_fingerprint)
        {
            self.notify_frontend(FrontendEvent::Error(
                "This device already belongs to another connection group".into(),
            ));
            return;
        }
        self.enrollment.stop();
        let listener =
            match tokio::net::TcpListener::bind(("0.0.0.0", port.unwrap_or(wire::PORT))).await {
                Ok(listener) => listener,
                Err(e) => {
                    self.notify_frontend(FrontendEvent::Error(format!(
                        "Cannot accept joining devices: {e}"
                    )));
                    self.publish_enrollment_state("failed");
                    return;
                }
            };
        let port = listener.local_addr().expect("listener address").port();
        let ips = local_ipv4();
        self.enrollment.addresses = ips.iter().map(|ip| format_address(*ip, port)).collect();
        let beacons = self.discovery.beacons(&ips, port);
        let identity = self.enrollment.identity.clone();
        let tx = self.enrollment.tx.clone();
        let epoch = self.enrollment.epoch;
        self.enrollment
            .ports
            .send_replace((self.port, self.data_addr.port()));
        let ports = self.enrollment.ports.subscribe();
        self.enrollment.task = Some(tokio::task::spawn_local(async move {
            let mut jobs = tokio::task::JoinSet::new();
            let mut announce = tokio::time::interval(discovery::INTERVAL);
            loop {
                tokio::select! {
                    _ = announce.tick() => beacons.send().await,
                    Some(_) = jobs.join_next(), if !jobs.is_empty() => {},
                    accepted = listener.accept(), if jobs.len() < 4 => {
                        let Ok((tcp, _)) = accepted else { break };
                        let identity = identity.clone();
                        let tx = tx.clone();
                        let ports = *ports.borrow();
                        jobs.spawn_local(async move {
                            let connection = wire::accept(tcp, &identity).await;
                            bounded_pair(connection, true, identity, ports, tx, epoch).await;
                        });
                    }
                }
            }
        }));
        self.publish_enrollment_state("hosting");
    }

    pub(super) fn join_connection_group(&mut self, address: String) {
        let address = match parse_address(&address) {
            Ok(address) => address,
            Err(reason) => {
                self.notify_frontend(FrontendEvent::Error(reason));
                return;
            }
        };
        self.enrollment.stop();
        let identity = self.enrollment.identity.clone();
        let tx = self.enrollment.tx.clone();
        let epoch = self.enrollment.epoch;
        let ports = (self.port, self.data_addr.port());
        self.enrollment.task = Some(tokio::task::spawn_local(async move {
            let connection = wire::connect(address, &identity).await;
            bounded_pair(connection, false, identity, ports, tx, epoch).await;
        }));
        self.publish_enrollment_state("joining");
    }

    pub(super) fn handle_enrollment_event(&mut self, event: Event) {
        self.enrollment
            .pending
            .retain(|_, decision| !decision.is_closed());
        match event {
            Event::Approval {
                epoch,
                id,
                peer,
                decision,
            } if epoch == self.enrollment.epoch => {
                self.enrollment.pending.insert(id, decision);
                self.notify_frontend(FrontendEvent::EnrollmentApproval {
                    id,
                    ip: peer.ip.to_string(),
                    fingerprint: peer.fingerprint,
                    local_fingerprint: self.public_key_fingerprint.clone(),
                });
            }
            Event::Accepted {
                epoch,
                peer,
                local,
                reply,
            } if epoch == self.enrollment.epoch => {
                if local.input_port != self.port {
                    let _ = reply.send(Err("Input port changed during pairing; join again".into()));
                    return;
                }
                let mut group = self.enrollment.group.clone().unwrap_or(Group {
                    leader: local.fingerprint.clone(),
                    revision: 1,
                    peers: vec![local.clone()],
                });
                if group.leader != self.public_key_fingerprint {
                    let _ = reply.send(Err("Not the group host".into()));
                    return;
                }
                if let Some(current) = group
                    .peers
                    .iter_mut()
                    .find(|p| p.fingerprint == local.fingerprint)
                {
                    *current = local;
                }
                group.peers.retain(|p| p.fingerprint != peer.fingerprint);
                group.peers.push(peer);
                let Some(revision) = group.revision.checked_add(1) else {
                    return;
                };
                group.revision = revision;
                if let Err(reason) = self.check_connection_group(&group) {
                    let _ = reply.send(Err(reason.into()));
                    return;
                }
                self.install_connection_group(group.clone());
                self.broadcast_connection_group();
                let _ = reply.send(Ok(group));
            }
            Event::Joined { epoch, group } if epoch == self.enrollment.epoch => {
                if let Err(reason) = self.check_connection_group(&group) {
                    self.publish_enrollment_state("failed");
                    self.notify_frontend(FrontendEvent::Error(format!(
                        "Could not join devices: {reason}"
                    )));
                    return;
                }
                self.install_connection_group(group);
                self.update_enrollment_port();
                self.publish_enrollment_state("joined");
            }
            Event::Failed { epoch, reason } if epoch == self.enrollment.epoch => {
                if self.enrollment.status != "hosting" {
                    self.publish_enrollment_state("failed");
                }
                self.notify_frontend(FrontendEvent::Error(format!(
                    "Could not join devices: {reason}"
                )));
            }
            _ => {}
        }
    }

    fn can_install_group(&self, group: &Group) -> bool {
        self.check_connection_group(group).is_ok()
    }

    fn check_connection_group(&self, group: &Group) -> Result<(), &'static str> {
        check_connection_group(
            group,
            &self.public_key_fingerprint,
            self.enrollment.group.as_ref(),
            &self.allowed_fingerprints.read().expect("lock"),
        )
    }

    pub(super) fn install_connection_group(&mut self, group: Group) {
        if !self.can_install_group(&group) {
            return;
        }
        let peers: Vec<_> = group
            .peers
            .iter()
            .filter(|p| p.fingerprint != self.public_key_fingerprint)
            .cloned()
            .collect();
        let allowed: HashSet<_> = peers.iter().map(|p| p.fingerprint.clone()).collect();
        for (handle, _, state) in self.client_manager.get_client_states() {
            if state
                .peer_fingerprint
                .as_ref()
                .is_some_and(|fp| !allowed.contains(fp))
            {
                self.remove_client(handle);
            }
        }
        *self.allowed_fingerprints.write().expect("lock") = allowed;
        *self.authorized_keys.write().expect("lock") = peers
            .iter()
            .map(|p| (p.fingerprint.clone(), p.ip.to_string()))
            .collect();
        self.close_revoked_data_sessions();
        self.sharing_addresses.clear();
        for (index, peer) in peers.iter().enumerate() {
            let existing = self
                .client_manager
                .get_client_states()
                .into_iter()
                .find(|(_, c, s)| {
                    s.peer_fingerprint.as_deref() == Some(&peer.fingerprint)
                        || (s.peer_fingerprint.is_none()
                            && c.fix_ips.contains(&IpAddr::V4(peer.ip))
                            && c.port == peer.input_port)
                })
                .map(|(h, _, _)| h);
            let handle = existing.unwrap_or_else(|| {
                self.client_manager.add_with_config(ConfigClient {
                    ips: HashSet::from([IpAddr::V4(peer.ip)]),
                    hostname: Some(peer.ip.to_string()),
                    port: peer.input_port,
                    pos: if index == 0 {
                        Position::Left
                    } else {
                        Position::Right
                    },
                    active: false,
                    enter_hook: None,
                    leave_hook: None,
                })
            });
            self.client_manager
                .set_fix_ips(handle, vec![IpAddr::V4(peer.ip)]);
            self.client_manager.set_port(handle, peer.input_port);
            self.client_manager
                .set_fingerprint(handle, peer.fingerprint.clone());
            if existing.is_none() {
                self.activate_client(handle);
            }
            self.broadcast_client(handle);
            self.sharing_addresses.insert(
                peer.fingerprint.clone(),
                SocketAddr::from((peer.ip, peer.data_port)),
            );
        }
        self.config.set_connection_group(group.clone());
        self.enrollment.group = Some(group);
        self.refresh_sharing_members();
        self.save_config();
        self.enumerate();
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
        self.retry_sharing_connections();
    }

    pub(super) fn receive_connection_group(&mut self, device: &str, group: Group) {
        let Some(old) = &self.enrollment.group else {
            return;
        };
        if old.leader == self.public_key_fingerprint && member_port_update(old, &group, device) {
            let mut updated = group;
            let Some(revision) = updated.revision.checked_add(1) else {
                return;
            };
            updated.revision = revision;
            self.install_connection_group(updated);
            self.broadcast_connection_group();
            return;
        }
        if old.leader != device || group.leader != device || group.revision <= old.revision {
            return;
        }
        self.install_connection_group(group);
        self.update_enrollment_port();
    }

    pub(super) fn update_enrollment_port(&mut self) {
        self.enrollment
            .ports
            .send_replace((self.port, self.data_addr.port()));
        let Some(mut group) = self.enrollment.group.clone() else {
            return;
        };
        let Some(local) = group
            .peers
            .iter_mut()
            .find(|p| p.fingerprint == self.public_key_fingerprint)
        else {
            return;
        };
        if self.port == 0 || local.input_port == self.port {
            return;
        }
        local.input_port = self.port;
        if !group.valid() {
            self.notify_frontend(FrontendEvent::Error(
                "Input port conflicts with another group member".into(),
            ));
            return;
        }
        if group.leader == self.public_key_fingerprint {
            let Some(revision) = group.revision.checked_add(1) else {
                return;
            };
            group.revision = revision;
            self.install_connection_group(group);
            self.broadcast_connection_group();
        } else {
            let leader = group.leader.clone();
            self.send_session_control(&leader, DataMessage::Group { group });
        }
    }

    pub(super) fn revoke_group_member(&mut self, fingerprint: &str) {
        let Some(mut group) = self.enrollment.group.clone() else {
            return;
        };
        if !group.peers.iter().any(|p| p.fingerprint == fingerprint) {
            return;
        }
        self.enrollment.stop();
        if group.leader == self.public_key_fingerprint {
            group.peers.retain(|p| p.fingerprint != fingerprint);
            if let Some(revision) = group.revision.checked_add(1) {
                group.revision = revision;
                self.install_connection_group(group);
                self.broadcast_connection_group();
            }
        } else {
            // A local revocation withdraws trust in future host membership updates.
            self.enrollment.group = None;
            self.config.clear_connection_group();
        }
        self.publish_enrollment_state("stopped");
    }

    pub(super) fn broadcast_connection_group(&mut self) {
        let Some(group) = self.enrollment.group.clone() else {
            return;
        };
        if group.leader != self.public_key_fingerprint {
            return;
        }
        for device in self.data_sessions.connected_devices() {
            self.send_session_control(
                &device,
                DataMessage::Group {
                    group: group.clone(),
                },
            );
        }
    }
}

fn check_connection_group(
    group: &Group,
    local_fingerprint: &str,
    old_group: Option<&Group>,
    allowed: &HashSet<String>,
) -> Result<(), &'static str> {
    group.validate()?;
    if !group.peers.iter().any(|p| p.fingerprint == local_fingerprint) {
        return Err("The connection group does not contain this device");
    }
    if old_group.is_some_and(|old| old.leader != group.leader) {
        return Err("This device already belongs to another connection group");
    }
    if allowed.iter().any(|fp| {
        !group.peers.iter().any(|p| &p.fingerprint == fp)
            && !old_group.is_some_and(|old| old.peers.iter().any(|p| &p.fingerprint == fp))
    }) {
        return Err(
            "Existing authorizations include devices outside this group; review and revoke unused authorizations before joining",
        );
    }
    Ok(())
}

fn member_port_update(current: &Group, proposed: &Group, source: &str) -> bool {
    proposed.valid()
        && proposed.leader == current.leader
        && proposed.revision == current.revision
        && proposed.peers.len() == current.peers.len()
        && source != current.leader
        && current.peers.iter().any(|p| p.fingerprint == source)
        && current.peers.iter().all(|old| {
            proposed
                .peers
                .iter()
                .find(|p| p.fingerprint == old.fingerprint)
                .is_some_and(|new| {
                    if old.fingerprint == source {
                        new.ip == old.ip
                            && new.data_port == old.data_port
                            && new.input_port != old.input_port
                    } else {
                        new == old
                    }
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joining_addresses_include_hotspot_and_filter_unusable_addresses() {
        let addresses = [
            "192.0.2.10",
            "192.168.137.1",
            "192.0.2.10",
            "127.0.0.1",
            "169.254.10.2",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
        ]
        .map(|ip| ip.parse::<IpAddr>().unwrap());
        assert_eq!(
            joining_addresses(addresses, wire::PORT),
            ["192.0.2.10", "192.168.137.1"]
        );
        assert_eq!(
            joining_addresses(addresses, 5000),
            ["192.0.2.10:5000", "192.168.137.1:5000"]
        );
    }

    #[test]
    fn group_admission_preserves_authorization_boundaries() {
        let peers: Vec<_> = (1..=3)
            .map(|id| Peer {
                fingerprint: vec![format!("{id:02x}"); 32].join(":"),
                ip: Ipv4Addr::new(192, 168, 137, id),
                input_port: 4242,
                data_port: 4243,
            })
            .collect();
        let group = Group {
            leader: peers[0].fingerprint.clone(),
            revision: 2,
            peers: peers[..2].to_vec(),
        };
        let local = &peers[0].fingerprint;
        assert!(check_connection_group(&group, local, None, &HashSet::new()).is_ok());
        let allowed = HashSet::from([peers[2].fingerprint.clone()]);
        assert!(check_connection_group(&group, local, None, &allowed)
            .unwrap_err()
            .contains("outside this group"));
        let old = Group {
            peers: peers.clone(),
            ..group.clone()
        };
        assert!(check_connection_group(&group, local, Some(&old), &allowed).is_ok());
        let other = Group {
            leader: peers[1].fingerprint.clone(),
            ..old
        };
        assert!(check_connection_group(&group, local, Some(&other), &allowed)
            .unwrap_err()
            .contains("another connection group"));
        assert!(
            check_connection_group(&group, &peers[2].fingerprint, None, &HashSet::new())
                .unwrap_err()
                .contains("does not contain")
        );
    }

    #[test]
    fn member_can_propose_only_its_own_input_port() {
        let peers: Vec<_> = (1..=3)
            .map(|id| Peer {
                fingerprint: vec![format!("{id:02x}"); 32].join(":"),
                ip: Ipv4Addr::LOCALHOST,
                input_port: 5000 + id,
                data_port: 6000 + id,
            })
            .collect();
        let current = Group {
            leader: peers[0].fingerprint.clone(),
            revision: 8,
            peers,
        };
        let source = &current.peers[1].fingerprint;
        let mut proposed = current.clone();
        proposed.peers[1].input_port = 7000;
        assert!(member_port_update(&current, &proposed, source));
        assert!(!member_port_update(&current, &proposed, &current.leader));
        assert!(!member_port_update(&current, &proposed, "unknown"));
        assert!(!member_port_update(&current, &current, source));

        let valid = proposed.clone();
        proposed.revision += 1;
        assert!(!member_port_update(&current, &proposed, source));
        proposed = valid.clone();
        proposed.peers[0].input_port += 10;
        assert!(!member_port_update(&current, &proposed, source));
        proposed = valid.clone();
        proposed.peers[1].ip = Ipv4Addr::new(127, 0, 0, 2);
        assert!(!member_port_update(&current, &proposed, source));
        proposed = valid.clone();
        proposed.peers[1].data_port += 10;
        assert!(!member_port_update(&current, &proposed, source));
        proposed = valid.clone();
        proposed.peers.pop();
        assert!(!member_port_update(&current, &proposed, source));
        proposed = valid.clone();
        proposed.leader = source.clone();
        assert!(!member_port_update(&current, &proposed, source));
        proposed = valid;
        proposed.peers[1].input_port = current.peers[0].input_port;
        assert!(!member_port_update(&current, &proposed, source));
    }

    #[test]
    fn address_is_explicit_ipv4_not_a_display_name() {
        assert_eq!(
            parse_address(" 192.168.1.8 ").unwrap(),
            "192.168.1.8:4244".parse().unwrap()
        );
        assert_eq!(parse_address("127.0.0.1:12345").unwrap().port(), 12345);
        for bad in [
            "my computer",
            "",
            "192.168.",
            "0.0.0.0",
            "224.0.0.1",
            "127.0.0.1:0",
        ] {
            assert!(parse_address(bad).is_err(), "{bad}");
        }
    }
}

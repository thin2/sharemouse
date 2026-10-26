//! Unanimous input sessions for a fixed group of authorized devices.
//!
//! The lowest fingerprint coordinates, but the user selects the primary.
//! A new session requires every member to drain native input first.
//! Missing members block progress; there is no timeout-based takeover.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Round {
    pub epoch: [u8; 16],
    pub sequence: u64,
}

impl Round {
    pub fn token(self) -> [u8; 24] {
        let mut token = [0; 24];
        token[..16].copy_from_slice(&self.epoch);
        token[16..].copy_from_slice(&self.sequence.to_be_bytes());
        token
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Message {
    Hello {
        members: BTreeSet<String>,
        epoch: [u8; 16],
    },
    Request {
        primary: Option<String>,
    },
    Prepare {
        round: Round,
        primary: Option<String>,
    },
    Prepared {
        round: Round,
    },
    Commit {
        round: Round,
    },
    Ready {
        round: Round,
    },
    Start {
        round: Round,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Send {
        device: String,
        message: Message,
    },
    /// Close both admission gates immediately, then drain native queues.
    Fence {
        round: Option<Round>,
    },
    Install {
        round: Round,
        primary: Option<String>,
    },
    Start {
        round: Round,
        primary: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Waiting,
    Stopping,
    Installing,
    Ready,
    Stopped,
    Running,
}

pub struct Sharing {
    local: String,
    members: BTreeSet<String>,
    epoch: [u8; 16],
    peers: BTreeMap<String, [u8; 16]>,
    round: Option<Round>,
    sequence: u64,
    primary: Option<String>,
    phase: Phase,
    votes: BTreeSet<String>,
}

impl Sharing {
    pub fn new(local: String, members: BTreeSet<String>, epoch: [u8; 16]) -> Self {
        assert!(members.contains(&local));
        Self {
            local,
            members,
            epoch,
            peers: BTreeMap::new(),
            round: None,
            sequence: 0,
            primary: None,
            phase: Phase::Waiting,
            votes: BTreeSet::new(),
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn round(&self) -> Option<Round> {
        self.round
    }
    pub fn primary(&self) -> Option<&str> {
        self.primary.as_deref()
    }
    pub fn members(&self) -> &BTreeSet<String> {
        &self.members
    }
    pub fn leader(&self) -> &str {
        self.members.first().expect("nonempty group")
    }
    pub fn hello(&self) -> Message {
        Message::Hello {
            members: self.members.clone(),
            epoch: self.epoch,
        }
    }
    fn is_leader(&self) -> bool {
        self.leader() == self.local
    }
    fn complete(&self) -> bool {
        self.members.len() <= 3 && self.peers.len() + 1 == self.members.len()
    }
    fn broadcast(&self, message: Message) -> Vec<Effect> {
        self.members
            .iter()
            .map(|device| Effect::Send {
                device: device.clone(),
                message: message.clone(),
            })
            .collect()
    }

    /// Called on replacement as well as disconnect. Connection generations
    /// must be checked by the caller before delivering network messages.
    pub fn disconnected(&mut self, device: &str) -> Vec<Effect> {
        self.peers.remove(device);
        self.primary = None;
        self.phase = Phase::Waiting;
        self.votes.clear();
        vec![Effect::Fence { round: None }]
    }

    pub fn request(&mut self, primary: Option<String>) -> Result<Vec<Effect>, &'static str> {
        if primary.as_ref().is_some_and(|p| !self.members.contains(p)) {
            return Err("primary is not a sharing group member");
        }
        if !self.complete() {
            return Err("connect every group member before selecting a primary");
        }
        if primary.is_some() && self.phase != Phase::Stopped {
            return Err("stop sharing and wait for every device to confirm first");
        }
        Ok(vec![Effect::Send {
            device: self.leader().into(),
            message: Message::Request { primary },
        }])
    }

    /// Begin the initial all-member stop barrier (also used after reconnect).
    pub fn initialize(&mut self) -> Vec<Effect> {
        if self.is_leader() && self.complete() {
            self.prepare(None)
        } else {
            vec![]
        }
    }

    fn prepare(&mut self, primary: Option<String>) -> Vec<Effect> {
        let Some(sequence) = self.sequence.checked_add(1) else {
            return self.disconnected(&self.local.clone());
        };
        self.sequence = sequence;
        self.broadcast(Message::Prepare {
            round: Round {
                epoch: self.epoch,
                sequence,
            },
            primary,
        })
    }

    /// Native capture and emulation queues have both drained successfully.
    pub fn released(&mut self, round: Round) -> Vec<Effect> {
        if self.round != Some(round) || self.phase != Phase::Stopping {
            return vec![];
        }
        vec![Effect::Send {
            device: self.leader().into(),
            message: Message::Prepared { round },
        }]
    }

    pub fn receive(&mut self, source: &str, message: Message) -> Vec<Effect> {
        if !self.members.contains(source) {
            return vec![];
        }
        if let Message::Hello { members, epoch } = message {
            if source == self.local {
                return vec![];
            }
            if members != self.members {
                return self.disconnected(source);
            }
            if self.peers.get(source) == Some(&epoch) {
                return vec![];
            }
            let mut effects = self.disconnected(source);
            self.peers.insert(source.into(), epoch);
            effects.push(Effect::Send {
                device: source.into(),
                message: self.hello(),
            });
            effects.extend(self.initialize());
            if !self.is_leader() && self.peers.contains_key(self.leader()) {
                effects.push(Effect::Send {
                    device: self.leader().into(),
                    message: Message::Request { primary: None },
                });
            }
            return effects;
        }
        if source != self.local && !self.peers.contains_key(source) {
            return vec![];
        }
        match message {
            Message::Request { primary } if self.is_leader() && self.complete() => {
                if primary.as_ref().is_some_and(|p| !self.members.contains(p)) {
                    return vec![];
                }
                // Stop is always allowed and supersedes a start in progress.
                if primary.is_none() || self.phase == Phase::Stopped {
                    self.prepare(primary)
                } else {
                    vec![]
                }
            }
            Message::Prepare { round, primary } if source == self.leader() => {
                // The coordinator's links can be ready before two followers
                // have completed their own Hello exchange. Such a follower
                // must not report Stopped (and enable Start) prematurely.
                // Its last Hello will request a fresh stop round.
                if !self.complete() {
                    return vec![];
                }
                let epoch = if self.is_leader() {
                    Some(&self.epoch)
                } else {
                    self.peers.get(source)
                };
                if epoch != Some(&round.epoch)
                    || self.round.is_some_and(|old| {
                        old.epoch == round.epoch && old.sequence >= round.sequence
                    })
                    || primary.as_ref().is_some_and(|p| !self.members.contains(p))
                {
                    return vec![];
                }
                self.round = Some(round);
                self.primary = primary;
                self.phase = Phase::Stopping;
                self.votes.clear();
                vec![Effect::Fence { round: Some(round) }]
            }
            Message::Prepared { round }
                if self.is_leader()
                    && self.round == Some(round)
                    && self.phase == Phase::Stopping =>
            {
                self.votes.insert(source.into());
                if self.votes == self.members {
                    // Change phase only on the looped-back Commit, otherwise
                    // the coordinator would skip its own installation.
                    self.broadcast(Message::Commit { round })
                } else {
                    vec![]
                }
            }
            Message::Commit { round }
                if source == self.leader()
                    && self.round == Some(round)
                    && self.phase == Phase::Stopping =>
            {
                self.phase = Phase::Installing;
                self.votes.clear();
                vec![
                    Effect::Install {
                        round,
                        primary: self.primary.clone(),
                    },
                    Effect::Send {
                        device: self.leader().into(),
                        message: Message::Ready { round },
                    },
                ]
            }
            Message::Ready { round }
                if self.is_leader()
                    && self.round == Some(round)
                    && self.phase == Phase::Installing =>
            {
                self.votes.insert(source.into());
                if self.votes == self.members {
                    self.phase = Phase::Ready;
                    self.broadcast(Message::Start { round })
                } else {
                    vec![]
                }
            }
            Message::Start { round }
                if source == self.leader()
                    && self.round == Some(round)
                    && matches!(self.phase, Phase::Installing | Phase::Ready) =>
            {
                self.phase = if self.primary.is_some() {
                    Phase::Running
                } else {
                    Phase::Stopped
                };
                vec![Effect::Start {
                    round,
                    primary: self.primary.clone(),
                }]
            }
            _ => vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn incomplete_mesh_cannot_confirm_a_stop_barrier() {
        let members = BTreeSet::from(["a".into(), "b".into(), "c".into()]);
        let mut follower = Sharing::new("c".into(), members, [2; 16]);
        follower.receive(
            "a",
            Message::Hello {
                members: follower.members.clone(),
                epoch: [0; 16],
            },
        );
        let round = Round {
            epoch: [0; 16],
            sequence: 1,
        };
        assert!(
            follower
                .receive(
                    "a",
                    Message::Prepare {
                        round,
                        primary: None
                    }
                )
                .is_empty()
        );
        assert!(follower.released(round).is_empty());
        assert!(follower.receive("a", Message::Commit { round }).is_empty());
        assert!(follower.receive("a", Message::Start { round }).is_empty());
        assert_eq!(follower.phase(), Phase::Waiting);
        follower.receive(
            "b",
            Message::Hello {
                members: follower.members.clone(),
                epoch: [1; 16],
            },
        );
        let round = Round {
            epoch: [0; 16],
            sequence: 2,
        };
        assert_eq!(
            follower.receive(
                "a",
                Message::Prepare {
                    round,
                    primary: None
                }
            ),
            [Effect::Fence { round: Some(round) }]
        );
        assert!(matches!(
            follower.released(round).as_slice(),
            [Effect::Send {
                message: Message::Prepared { .. },
                ..
            }]
        ));
    }

    struct Cluster {
        nodes: BTreeMap<String, Sharing>,
        wire: VecDeque<(String, String, Message)>,
        withheld: Vec<(String, Round)>,
        hold: Option<String>,
        running: BTreeMap<String, String>,
    }

    impl Cluster {
        fn new() -> Self {
            let members: BTreeSet<String> = BTreeSet::from(["a".into(), "b".into(), "c".into()]);
            let nodes = members
                .iter()
                .enumerate()
                .map(|(i, id)| {
                    (
                        id.clone(),
                        Sharing::new(id.clone(), members.clone(), [i as u8; 16]),
                    )
                })
                .collect();
            let mut cluster = Self {
                nodes,
                wire: VecDeque::new(),
                withheld: vec![],
                hold: None,
                running: BTreeMap::new(),
            };
            for source in ["a", "b", "c"] {
                for target in ["a", "b", "c"] {
                    if source != target {
                        cluster.wire.push_back((
                            source.into(),
                            target.into(),
                            cluster.nodes[source].hello(),
                        ));
                    }
                }
            }
            cluster.pump();
            assert!(cluster.nodes.values().all(|s| s.phase() == Phase::Stopped));
            cluster
        }
        fn effects(&mut self, local: &str, effects: Vec<Effect>) {
            for effect in effects {
                match effect {
                    Effect::Send { device, message } => {
                        self.wire.push_back((local.into(), device, message))
                    }
                    Effect::Fence { round } => {
                        self.running.remove(local);
                        if let Some(round) = round {
                            if self.hold.as_deref() == Some(local) {
                                self.withheld.push((local.into(), round));
                            } else {
                                let effects = self.nodes.get_mut(local).unwrap().released(round);
                                self.effects(local, effects);
                            }
                        }
                    }
                    Effect::Install { .. } => {}
                    Effect::Start { primary, .. } => {
                        if primary.as_deref() == Some(local) {
                            self.running.insert(local.into(), local.into());
                        }
                    }
                }
                assert!(self.running.len() <= 1, "two primary senders were admitted");
            }
        }
        fn pump(&mut self) {
            let mut count = 0;
            while let Some((source, target, message)) = self.wire.pop_front() {
                count += 1;
                assert!(count < 1000, "coordination did not settle");
                let effects = self
                    .nodes
                    .get_mut(&target)
                    .unwrap()
                    .receive(&source, message);
                self.effects(&target, effects);
            }
        }
        fn request(&mut self, source: &str, primary: Option<&str>) {
            let effects = self
                .nodes
                .get_mut(source)
                .unwrap()
                .request(primary.map(str::to_owned))
                .unwrap();
            self.effects(source, effects);
            self.pump();
        }
    }

    #[test]
    fn all_members_must_release_before_handover() {
        let mut c = Cluster::new();
        c.request("b", Some("b"));
        assert_eq!(c.running.keys().cloned().collect::<Vec<_>>(), ["b"]);
        c.hold = Some("c".into());
        c.request("a", None);
        assert!(c.running.is_empty());
        assert!(
            c.nodes
                .get_mut("a")
                .unwrap()
                .request(Some("a".into()))
                .is_err()
        );
        assert_eq!(c.nodes["a"].phase(), Phase::Stopping);
        c.hold = None;
        for (device, round) in std::mem::take(&mut c.withheld) {
            let effects = c.nodes.get_mut(&device).unwrap().released(round);
            c.effects(&device, effects);
        }
        c.pump();
        c.request("a", Some("a"));
        assert_eq!(c.running.keys().cloned().collect::<Vec<_>>(), ["a"]);
    }

    #[test]
    fn simultaneous_primary_requests_choose_only_one() {
        let mut c = Cluster::new();
        for source in ["b", "c"] {
            let effects = c
                .nodes
                .get_mut(source)
                .unwrap()
                .request(Some(source.into()))
                .unwrap();
            c.effects(source, effects);
        }
        c.pump();
        assert_eq!(c.running.len(), 1);
        let primary = c.nodes["a"].primary().unwrap();
        assert!(c.nodes.values().all(|s| s.primary() == Some(primary)));
    }

    #[test]
    fn stale_confirmations_and_start_cannot_reopen_stopped_session() {
        let mut c = Cluster::new();
        c.request("a", Some("a"));
        let old = c.nodes["a"].round.unwrap();
        c.request("b", None);
        for node in c.nodes.values_mut() {
            for message in [
                Message::Prepared { round: old },
                Message::Ready { round: old },
                Message::Commit { round: old },
                Message::Start { round: old },
                Message::Prepare {
                    round: old,
                    primary: Some("a".into()),
                },
            ] {
                assert!(node.receive("a", message).is_empty());
            }
            assert_eq!(node.phase(), Phase::Stopped);
        }
    }

    #[test]
    fn disconnect_blocks_takeover_and_reconnect_requires_release_again() {
        let mut c = Cluster::new();
        c.request("a", Some("a"));
        let old = c.nodes["a"].round.unwrap();
        for node in ["a", "b"] {
            let effects = c.nodes.get_mut(node).unwrap().disconnected("c");
            c.effects(node, effects);
        }
        assert!(
            c.nodes
                .get_mut("b")
                .unwrap()
                .request(Some("b".into()))
                .is_err()
        );
        assert!(
            c.nodes
                .get_mut("a")
                .unwrap()
                .receive("a", Message::Start { round: old })
                .is_empty()
        );
        c.hold = Some("c".into());
        let hello = c.nodes["c"].hello();
        let effects = c.nodes.get_mut("a").unwrap().receive("c", hello);
        c.effects("a", effects);
        c.pump();
        assert!(c.running.is_empty());
        assert_eq!(c.nodes["a"].phase(), Phase::Stopping);
    }

    #[test]
    fn membership_mismatch_and_non_coordinator_cannot_install_sessions() {
        let mut c = Cluster::new();
        let round = Round {
            epoch: [8; 16],
            sequence: 100,
        };
        assert!(
            c.nodes
                .get_mut("b")
                .unwrap()
                .receive(
                    "c",
                    Message::Prepare {
                        round,
                        primary: Some("c".into())
                    }
                )
                .is_empty()
        );
        let effects = c.nodes.get_mut("a").unwrap().receive(
            "b",
            Message::Hello {
                members: BTreeSet::from(["a".into(), "b".into()]),
                epoch: [1; 16],
            },
        );
        assert_eq!(effects, [Effect::Fence { round: None }]);
        assert!(
            c.nodes
                .get_mut("a")
                .unwrap()
                .request(Some("a".into()))
                .is_err()
        );
    }

    #[test]
    fn restarted_coordinator_fences_old_epoch() {
        let mut c = Cluster::new();
        c.request("b", Some("b"));
        let old = c.nodes["b"].round.unwrap();
        c.nodes.insert(
            "a".into(),
            Sharing::new("a".into(), c.nodes["b"].members.clone(), [9; 16]),
        );
        let hello = c.nodes["a"].hello();
        for target in ["b", "c"] {
            let effects = c.nodes.get_mut(target).unwrap().receive("a", hello.clone());
            c.effects(target, effects);
        }
        c.pump();
        assert!(c.running.is_empty());
        assert!(c.nodes.values().all(|s| s.phase() == Phase::Stopped));
        assert!(
            c.nodes
                .get_mut("b")
                .unwrap()
                .receive("a", Message::Start { round: old })
                .is_empty()
        );
        c.request("c", Some("c"));
        assert_eq!(c.running.keys().cloned().collect::<Vec<_>>(), ["c"]);
    }
}

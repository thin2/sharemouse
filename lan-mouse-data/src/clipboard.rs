//! Clipboard coordination, independent of native clipboard and transport APIs.
//!
//! The selected primary serializes source announcements in its event loop.
//! Its acceptance order is authoritative, including concurrent copies and
//! copies discovered after a partition; wall clocks never participate.
//! Metadata can reach the primary from every source, but only the current
//! target requests the body. A new primary must explicitly start a fresh
//! session with a unique nonce, optionally carrying the latest metadata.
//!
//! Callers authenticate sources before `connect_source`, cache bodies by
//! `CopyId`, and compute the received body's SHA-256 before `can_apply`.
//! Check `can_apply` immediately before writing on the native clipboard's
//! event loop, then call `applied` with the resulting native change serial.
//! Native adapters and network dispatch are separate from this state machine.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A fresh, externally negotiated primary session. Never reuse the nonce.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionId {
    pub primary: String,
    pub nonce: String,
}

/// Source-local identity, independent of primary changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyId {
    pub source: String,
    /// Unique for each source process. Installed only by an authenticated
    /// connection, so delayed events cannot resurrect an older process.
    pub incarnation: String,
    pub sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    pub session: SessionId,
    pub sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Offer {
    pub version: Version,
    pub copy: CopyId,
    pub digest: [u8; 32],
}

/// Identifies both the copy and the particular visit to a target.
/// Even A -> B -> A must invalidate the first visit's unfinished request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchRequest {
    pub offer: Offer,
    pub target: String,
    ticket: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Idle,
    Current,
    WaitingForCopy { device: String },
    Unavailable { device: String },
    Fetch(FetchRequest),
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ClipboardError {
    #[error("device is not a member of this clipboard session")]
    UnknownDevice,
    #[error("clipboard source is disconnected or belongs to an old process")]
    StaleSource,
    #[error("clipboard session or source incarnation is empty")]
    EmptyIdentity,
    #[error("a new primary session must use a different identity")]
    ReusedSession,
    #[error("clipboard logical counter exhausted")]
    CounterExhausted,
}

struct Source {
    incarnation: String,
    sequence: u64,
    online: bool,
    copy_pending: bool,
}

struct WriteGuard {
    serial: u64,
    digest: [u8; 32],
}

pub struct ClipboardRouter {
    session: SessionId,
    members: HashSet<String>,
    sources: HashMap<String, Source>,
    sequence: u64,
    ticket: u64,
    latest: Option<Offer>,
    target: Option<String>,
    pending: Option<FetchRequest>,
    installed: HashMap<String, Version>,
    writes: HashMap<String, WriteGuard>,
}

impl ClipboardRouter {
    pub fn new(
        session: SessionId,
        members: impl IntoIterator<Item = String>,
    ) -> Result<Self, ClipboardError> {
        let members: HashSet<_> = members.into_iter().collect();
        validate_session(&session, &members)?;
        Ok(Self {
            session,
            members,
            sources: HashMap::new(),
            sequence: 0,
            ticket: 0,
            latest: None,
            target: None,
            pending: None,
            installed: HashMap::new(),
            writes: HashMap::new(),
        })
    }

    /// `device` is the authenticated identity, never an unverified payload.
    /// Reconnecting the same process preserves its high-water mark.
    pub fn connect_source(
        &mut self,
        device: &str,
        incarnation: &str,
    ) -> Result<(), ClipboardError> {
        self.check_member(device)?;
        if incarnation.is_empty() {
            return Err(ClipboardError::EmptyIdentity);
        }
        match self.sources.get_mut(device) {
            Some(source) if source.incarnation == incarnation => source.online = true,
            _ => {
                self.disconnect_source(device);
                self.writes.remove(device);
                self.sources.insert(
                    device.to_owned(),
                    Source {
                        incarnation: incarnation.to_owned(),
                        sequence: 0,
                        online: true,
                        copy_pending: false,
                    },
                );
            }
        }
        Ok(())
    }

    /// Keep latest metadata while offline; never silently revert to an older copy.
    pub fn disconnect_source(&mut self, device: &str) {
        if let Some(source) = self.sources.get_mut(device) {
            source.online = false;
        }
        self.installed.remove(device);
        if self
            .pending
            .as_ref()
            .is_some_and(|r| r.target == device || r.offer.copy.source == device)
        {
            self.pending = None;
        }
    }

    /// Accept one proactive copy, or one explicit "copy from history" action.
    /// Messages and remote clipboard writes must not call this method.
    /// Returns `None` for duplicate or out-of-order source announcements.
    pub fn announce(
        &mut self,
        authenticated_source: &str,
        copy: CopyId,
        digest: [u8; 32],
    ) -> Result<Option<Version>, ClipboardError> {
        self.check_member(authenticated_source)?;
        if authenticated_source != copy.source {
            return Err(ClipboardError::UnknownDevice);
        }
        let source = self
            .sources
            .get_mut(authenticated_source)
            .filter(|s| s.online && s.incarnation == copy.incarnation)
            .ok_or(ClipboardError::StaleSource)?;
        if copy.sequence <= source.sequence {
            return Ok(None);
        }
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or(ClipboardError::CounterExhausted)?;
        source.sequence = copy.sequence;
        source.copy_pending = false;
        self.sequence = sequence;
        let version = Version {
            session: self.session.clone(),
            sequence,
        };
        // A new copy invalidates our knowledge of the source's old clipboard.
        self.installed.insert(copy.source.clone(), version.clone());
        self.latest = Some(Offer {
            version: version.clone(),
            copy,
            digest,
        });
        self.pending = None;
        Ok(Some(version))
    }

    pub fn latest(&self) -> Option<&Offer> {
        self.latest.as_ref()
    }

    /// Restore replicated metadata when a newly selected primary constructs
    /// its router. Bodies remain at their sources; old source incarnations
    /// remain unavailable rather than falling back to an older clipboard.
    pub fn resume_metadata(
        &mut self,
        latest: Option<Offer>,
        accepted: impl IntoIterator<Item = CopyId>,
    ) -> Result<(), ClipboardError> {
        if self.sequence != 0 || self.latest.is_some() {
            return Err(ClipboardError::ReusedSession);
        }
        for copy in accepted {
            self.check_member(&copy.source)?;
            if let Some(source) = self.sources.get_mut(&copy.source) {
                if source.incarnation == copy.incarnation {
                    source.sequence = source.sequence.max(copy.sequence);
                }
            }
        }
        if let Some(mut offer) = latest {
            self.check_member(&offer.copy.source)?;
            self.sequence = 1;
            offer.version = Version {
                session: self.session.clone(),
                sequence: 1,
            };
            self.latest = Some(offer);
        }
        Ok(())
    }

    /// `None` pauses routing. The caller supplies the primary's own identity
    /// when the pointer returns to its local screen.
    pub fn set_target(&mut self, target: Option<String>) -> Result<(), ClipboardError> {
        if let Some(device) = &target {
            self.check_member(device)?;
        }
        if self.target != target {
            self.target = target;
            self.pending = None;
        }
        Ok(())
    }

    /// Poll after target, source availability or latest metadata changes.
    /// A returned request is stable until it completes or becomes stale.
    pub fn route(&mut self) -> Result<Route, ClipboardError> {
        let (Some(target), Some(offer)) = (&self.target, &self.latest) else {
            return Ok(Route::Idle);
        };
        if !self.online(target) {
            return Ok(Route::Unavailable {
                device: target.clone(),
            });
        }
        if self.sources.get(target).is_some_and(|s| s.copy_pending) {
            return Ok(Route::WaitingForCopy {
                device: target.clone(),
            });
        }
        if self.installed.get(target) == Some(&offer.version) {
            return Ok(Route::Current);
        }
        if !self.source_available(&offer.copy) {
            return Ok(Route::Unavailable {
                device: offer.copy.source.clone(),
            });
        }
        if let Some(request) = &self.pending {
            return Ok(Route::Fetch(request.clone()));
        }
        let ticket = self
            .ticket
            .checked_add(1)
            .ok_or(ClipboardError::CounterExhausted)?;
        self.ticket = ticket;
        let request = FetchRequest {
            offer: offer.clone(),
            target: target.clone(),
            ticket,
        };
        self.pending = Some(request.clone());
        Ok(Route::Fetch(request))
    }

    /// Must be rechecked after any asynchronous work, including image decoding.
    pub fn can_apply(&self, request: &FetchRequest, digest: [u8; 32]) -> bool {
        self.pending.as_ref() == Some(request)
            && self.latest.as_ref() == Some(&request.offer)
            && self.target.as_deref() == Some(request.target.as_str())
            && self.online(&request.target)
            && !self
                .sources
                .get(&request.target)
                .is_some_and(|s| s.copy_pending)
            && self.source_available(&request.offer.copy)
            && request.offer.digest == digest
    }

    /// Record a successful native write. A failed write leaves the route retryable.
    pub fn applied(
        &mut self,
        request: &FetchRequest,
        digest: [u8; 32],
        native_serial: u64,
    ) -> bool {
        if !self.can_apply(request, digest) {
            return false;
        }
        self.installed
            .insert(request.target.clone(), request.offer.version.clone());
        self.writes.insert(
            request.target.clone(),
            WriteGuard {
                serial: native_serial,
                digest,
            },
        );
        self.pending = None;
        true
    }

    /// `true` means a new native copy may be announced. Native serials must
    /// increase per device process. Repeated callbacks for a program write,
    /// including callbacks queued before that write, are suppressed.
    /// A user explicitly copying the same bytes again has a new serial.
    pub fn observe_native_change(
        &mut self,
        device: &str,
        incarnation: &str,
        native_serial: u64,
        digest: [u8; 32],
    ) -> Result<bool, ClipboardError> {
        self.check_member(device)?;
        if !self
            .sources
            .get(device)
            .is_some_and(|s| s.online && s.incarnation == incarnation)
        {
            return Err(ClipboardError::StaleSource);
        }
        if let Some(write) = self.writes.get(device) {
            if native_serial < write.serial
                || (native_serial == write.serial && digest == write.digest)
            {
                return Ok(false);
            }
            self.writes.remove(device);
        }
        // A genuine local copy makes a formerly installed version obsolete,
        // even before its new metadata is accepted by the primary.
        self.installed.remove(device);
        self.sources
            .get_mut(device)
            .expect("registered source")
            .copy_pending = true;
        if self.pending.as_ref().is_some_and(|r| r.target == device) {
            self.pending = None;
        }
        Ok(true)
    }

    /// A handover retains the latest content identity, but never reuses its
    /// old version or in-flight route. It does not write any system clipboard.
    /// Membership changes require a new router and authenticated registration.
    pub fn begin_session(&mut self, session: SessionId) -> Result<(), ClipboardError> {
        validate_session(&session, &self.members)?;
        if session == self.session {
            return Err(ClipboardError::ReusedSession);
        }
        self.session = session;
        self.sequence = u64::from(self.latest.is_some());
        if let Some(offer) = &mut self.latest {
            offer.version = Version {
                session: self.session.clone(),
                sequence: self.sequence,
            };
        }
        self.pending = None;
        self.target = None;
        self.installed.clear();
        Ok(())
    }

    fn check_member(&self, device: &str) -> Result<(), ClipboardError> {
        if self.members.contains(device) {
            Ok(())
        } else {
            Err(ClipboardError::UnknownDevice)
        }
    }

    fn online(&self, device: &str) -> bool {
        self.sources.get(device).is_some_and(|s| s.online)
    }

    fn source_available(&self, copy: &CopyId) -> bool {
        self.sources
            .get(&copy.source)
            .is_some_and(|s| s.online && s.incarnation == copy.incarnation)
    }
}

fn validate_session(session: &SessionId, members: &HashSet<String>) -> Result<(), ClipboardError> {
    if session.primary.is_empty()
        || session.nonce.is_empty()
        || members.iter().any(String::is_empty)
    {
        Err(ClipboardError::EmptyIdentity)
    } else if !members.contains(&session.primary) {
        Err(ClipboardError::UnknownDevice)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: [u8; 32] = [7; 32];

    fn session(primary: &str, nonce: &str) -> SessionId {
        SessionId {
            primary: primary.into(),
            nonce: nonce.into(),
        }
    }

    fn router() -> ClipboardRouter {
        let mut router =
            ClipboardRouter::new(session("a", "session-1"), ["a", "b", "c"].map(String::from))
                .unwrap();
        for device in ["a", "b", "c"] {
            router.connect_source(device, "boot-1").unwrap();
        }
        router
    }

    fn copy(source: &str, sequence: u64) -> CopyId {
        CopyId {
            source: source.into(),
            incarnation: "boot-1".into(),
            sequence,
        }
    }

    fn fetch(router: &mut ClipboardRouter, target: &str) -> FetchRequest {
        router.set_target(Some(target.into())).unwrap();
        match router.route().unwrap() {
            Route::Fetch(request) => request,
            other => panic!("expected fetch, got {other:?}"),
        }
    }

    #[test]
    fn a_to_b_to_c_requests_only_the_current_target_and_keeps_the_copy() {
        let mut r = router();
        r.announce("a", copy("a", 1), DIGEST).unwrap();
        assert_eq!(r.route().unwrap(), Route::Idle);
        let b = fetch(&mut r, "b");
        assert_eq!(r.route().unwrap(), Route::Fetch(b.clone()));
        assert!(r.applied(&b, DIGEST, 10));
        assert_eq!(r.route().unwrap(), Route::Current);
        let c = fetch(&mut r, "c");
        assert_eq!(b.offer, c.offer);
        assert_eq!(c.offer.copy.source, "a");
        assert!(r.applied(&c, DIGEST, 11));
        r.set_target(Some("b".into())).unwrap();
        assert_eq!(r.route().unwrap(), Route::Current);
    }

    #[test]
    fn a_new_copy_from_b_replaces_a_and_invalidates_its_download() {
        let mut r = router();
        r.announce("a", copy("a", 1), DIGEST).unwrap();
        let old = fetch(&mut r, "c");
        r.announce("b", copy("b", 1), [8; 32]).unwrap();
        assert!(!r.applied(&old, DIGEST, 10));
        let new = fetch(&mut r, "c");
        assert_eq!(new.offer.copy.source, "b");
        assert_eq!(new.offer.version.sequence, 2);
        assert!(r.applied(&new, [8; 32], 11));
    }

    #[test]
    fn concurrent_sources_use_primary_acceptance_order_without_wall_clocks() {
        for order in [["a", "b"], ["b", "a"]] {
            let mut r = router();
            for source in order {
                r.announce(source, copy(source, 1), DIGEST).unwrap();
            }
            assert_eq!(r.latest().unwrap().copy.source, order[1]);
            assert_eq!(r.latest().unwrap().version.sequence, 2);
        }
    }

    #[test]
    fn replayed_and_out_of_order_source_announcements_do_not_advance_version() {
        let mut r = router();
        r.announce("a", copy("a", 8), DIGEST).unwrap();
        for sequence in [8, 7, 0] {
            assert_eq!(r.announce("a", copy("a", sequence), DIGEST), Ok(None));
        }
        assert_eq!(r.latest().unwrap().version.sequence, 1);
    }

    #[test]
    fn source_cannot_claim_another_device_or_reenter_with_an_old_boot() {
        let mut r = router();
        assert_eq!(
            r.announce("b", copy("a", 1), DIGEST),
            Err(ClipboardError::UnknownDevice)
        );
        r.connect_source("a", "boot-2").unwrap();
        assert_eq!(
            r.announce("a", copy("a", 9), DIGEST),
            Err(ClipboardError::StaleSource)
        );
        let mut new = copy("a", 1);
        new.incarnation = "boot-2".into();
        assert!(r.announce("a", new, DIGEST).unwrap().is_some());
        assert_eq!(
            r.connect_source("stranger", "boot"),
            Err(ClipboardError::UnknownDevice)
        );
    }

    #[test]
    fn a_target_round_trip_invalidates_the_first_visits_download() {
        let mut r = router();
        r.announce("a", copy("a", 1), DIGEST).unwrap();
        let first = fetch(&mut r, "b");
        fetch(&mut r, "c");
        let second = fetch(&mut r, "b");
        assert_ne!(first, second);
        assert!(!r.can_apply(&first, DIGEST));
        assert!(r.can_apply(&second, DIGEST));
    }

    #[test]
    fn pause_and_wrong_content_cannot_write_the_clipboard() {
        let mut r = router();
        r.announce("a", copy("a", 1), DIGEST).unwrap();
        let request = fetch(&mut r, "b");
        assert!(!r.applied(&request, [9; 32], 10));
        assert_eq!(r.route().unwrap(), Route::Fetch(request.clone()));
        r.set_target(None).unwrap();
        assert!(!r.applied(&request, DIGEST, 10));
        assert_eq!(r.route().unwrap(), Route::Idle);
    }

    #[test]
    fn a_partition_keeps_metadata_and_recovery_retries_without_replaying_copies() {
        let mut r = router();
        r.announce("a", copy("a", 2), DIGEST).unwrap();
        let first = fetch(&mut r, "b");
        r.disconnect_source("a");
        assert_eq!(
            r.route().unwrap(),
            Route::Unavailable { device: "a".into() }
        );
        assert!(!r.can_apply(&first, DIGEST));
        r.connect_source("a", "boot-1").unwrap();
        assert_eq!(r.announce("a", copy("a", 2), DIGEST), Ok(None));
        let second = fetch(&mut r, "b");
        assert_ne!(first, second);
        assert_eq!(first.offer, second.offer);
        // A proactive copy discovered after recovery is sequenced on acceptance.
        r.announce("a", copy("a", 3), DIGEST).unwrap();
        assert_eq!(r.latest().unwrap().version.sequence, 2);
    }

    #[test]
    fn target_disconnect_invalidates_cached_installation_and_pending_work() {
        let mut r = router();
        r.announce("a", copy("a", 1), DIGEST).unwrap();
        let request = fetch(&mut r, "b");
        assert!(r.applied(&request, DIGEST, 10));
        r.disconnect_source("b");
        assert_eq!(
            r.route().unwrap(),
            Route::Unavailable { device: "b".into() }
        );
        r.connect_source("b", "boot-1").unwrap();
        assert_ne!(fetch(&mut r, "b"), request);
    }

    #[test]
    fn a_new_primary_rebases_metadata_and_rejects_old_session_completion() {
        let mut r = router();
        r.announce("a", copy("a", 1), DIGEST).unwrap();
        let old = fetch(&mut r, "c");
        r.begin_session(session("b", "session-2")).unwrap();
        assert_eq!(r.route().unwrap(), Route::Idle);
        assert!(!r.can_apply(&old, DIGEST));
        let new = fetch(&mut r, "c");
        assert_eq!(new.offer.copy, old.offer.copy);
        assert_ne!(new.offer.version.session, old.offer.version.session);
        assert_eq!(r.announce("a", copy("a", 1), DIGEST), Ok(None));
        assert_eq!(
            r.begin_session(session("b", "session-2")),
            Err(ClipboardError::ReusedSession)
        );
    }

    #[test]
    fn repeated_native_write_callbacks_are_suppressed_but_user_copy_is_new() {
        let mut r = router();
        r.announce("a", copy("a", 1), DIGEST).unwrap();
        let request = fetch(&mut r, "b");
        assert!(r.applied(&request, DIGEST, 10));
        for serial in [9, 10, 10] {
            assert_eq!(
                r.observe_native_change("b", "boot-1", serial, DIGEST),
                Ok(false)
            );
        }
        // The same bytes explicitly copied again have a different native serial.
        assert_eq!(r.observe_native_change("b", "boot-1", 11, DIGEST), Ok(true));
        r.announce("b", copy("b", 1), DIGEST).unwrap();
        assert_eq!(r.latest().unwrap().version.sequence, 2);
    }

    #[test]
    fn logical_counter_exhaustion_fails_without_accepting_a_partial_copy() {
        let mut r = router();
        r.sequence = u64::MAX;
        assert_eq!(
            r.announce("a", copy("a", 1), DIGEST),
            Err(ClipboardError::CounterExhausted)
        );
        assert!(r.latest().is_none());
        assert_eq!(r.sources["a"].sequence, 0);
    }

    #[test]
    fn a_local_copy_blocks_old_downloads_before_its_metadata_arrives() {
        let mut r = router();
        r.announce("a", copy("a", 1), DIGEST).unwrap();
        let old = fetch(&mut r, "b");
        assert_eq!(r.observe_native_change("b", "boot-1", 1, [9; 32]), Ok(true));
        assert!(!r.can_apply(&old, DIGEST));
        assert_eq!(
            r.route().unwrap(),
            Route::WaitingForCopy { device: "b".into() }
        );
        r.announce("b", copy("b", 1), [9; 32]).unwrap();
        assert_eq!(r.route().unwrap(), Route::Current);
    }

    #[test]
    fn restarting_a_target_discards_its_old_native_write_guard() {
        let mut r = router();
        r.announce("a", copy("a", 1), DIGEST).unwrap();
        let request = fetch(&mut r, "b");
        assert!(r.applied(&request, DIGEST, 100));
        r.connect_source("b", "boot-2").unwrap();
        assert_eq!(
            r.observe_native_change("b", "boot-1", 101, DIGEST),
            Err(ClipboardError::StaleSource)
        );
        assert_eq!(r.observe_native_change("b", "boot-2", 1, DIGEST), Ok(true));
    }
}

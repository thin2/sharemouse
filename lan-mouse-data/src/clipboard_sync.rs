//! Text/image clipboard dispatch with target permissions and bounded assembly.
use crate::clipboard::{ClipboardRouter, CopyId, FetchRequest, Offer, Route, SessionId};
use crate::clipboard_image::{Image, MAX_ENCODED, PART_BYTES};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};

#[cfg(test)]
mod tests;

pub const MAX_TEXT: usize = 128 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metadata {
    pub copy: CopyId,
    pub digest: [u8; 32],
    pub supported: bool,
    pub native_serial: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Message {
    Hello {
        incarnation: String,
    },
    Announce {
        session: SessionId,
        metadata: Metadata,
    },
    RequestMetadata {
        session: SessionId,
    },
    RequestLatest {
        session: SessionId,
    },
    Latest {
        offer: Offer,
        supported: bool,
    },
    Route {
        session: SessionId,
        revision: u64,
        request: Option<FetchRequest>,
        status: Status,
    },
    Fetch {
        request: FetchRequest,
    },
    Body {
        request: FetchRequest,
        text: Option<String>,
    },
    Apply {
        revision: u64,
        request: FetchRequest,
        text: String,
    },
    Applied {
        revision: u64,
        request: FetchRequest,
        native_serial: u64,
    },
    ImagePart {
        request: FetchRequest,
        /// None is source -> primary; Some is primary -> target.
        revision: Option<u64>,
        offset: u32,
        total: u32,
        bytes: Vec<u8>,
    },
}

impl Message {
    pub fn validate(&self) -> bool {
        // Bound even manually constructed identities before serialization.
        fn identity(s: &str) -> bool {
            !s.is_empty() && s.len() <= 128
        }
        fn session(s: &SessionId) -> bool {
            identity(&s.primary) && identity(&s.nonce)
        }
        fn copy(c: &CopyId) -> bool {
            identity(&c.source) && identity(&c.incarnation) && c.sequence > 0
        }
        fn offer(o: &Offer) -> bool {
            session(&o.version.session) && copy(&o.copy) && o.version.sequence > 0
        }
        fn request(r: &FetchRequest) -> bool {
            offer(&r.offer) && identity(&r.target)
        }
        match self {
            Self::Hello { incarnation } => identity(incarnation),
            Self::Announce {
                session: s,
                metadata,
            } => session(s) && copy(&metadata.copy),
            Self::RequestMetadata { session: s } | Self::RequestLatest { session: s } => session(s),
            Self::Latest { offer: o, .. } => offer(o),
            Self::Route {
                session: s,
                request: r,
                ..
            } => session(s) && r.as_ref().is_none_or(request),
            Self::Fetch { request: r } | Self::Applied { request: r, .. } => request(r),
            Self::Body { request: r, text } => {
                request(r) && text.as_ref().is_none_or(|t| t.len() <= MAX_TEXT)
            }
            Self::Apply {
                request: r, text, ..
            } => request(r) && text.len() <= MAX_TEXT,
            Self::ImagePart {
                request: r,
                offset,
                total,
                bytes,
                ..
            } => {
                request(r)
                    && *total > 0
                    && *total as usize <= MAX_ENCODED
                    && !bytes.is_empty()
                    && bytes.len() <= PART_BYTES
                    && u64::from(*offset) + bytes.len() as u64 <= u64::from(*total)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    Paused,
    Waiting,
    Syncing,
    Ready,
    Unavailable,
    Unsupported,
    Failed,
}

pub enum Action {
    CancelImages,
    SendImage {
        device: String,
        request: FetchRequest,
        revision: Option<u64>,
        image: Image,
    },
    WriteImage {
        revision: u64,
        request: FetchRequest,
        image: Image,
        expected_serial: u64,
    },
    Send {
        device: String,
        message: Message,
    },
    Write {
        revision: u64,
        request: FetchRequest,
        text: String,
        expected_serial: u64,
    },
    Status(Status),
}

pub struct ClipboardSync {
    local: String,
    incarnation: String,
    members: HashSet<String>,
    peers: HashMap<String, (u64, Option<String>)>,
    session: Option<SessionId>,
    router: Option<ClipboardRouter>,
    latest: Option<(Offer, bool)>,
    accepted: HashMap<String, CopyId>,
    local_pending: Option<Metadata>,
    cache: VecDeque<(CopyId, Option<String>)>,
    images: VecDeque<(CopyId, Image)>,
    receiving: Option<Assembly>,
    sequence: u64,
    serial: Option<u64>,
    target: Option<String>,
    revision: u64,
    incoming_revision: u64,
    incoming: Option<FetchRequest>,
    route_sent: Option<(Option<String>, Option<FetchRequest>, Status)>,
    fetch_sent: Option<FetchRequest>,
    unavailable: Option<FetchRequest>,
}
struct Assembly {
    request: FetchRequest,
    revision: Option<u64>,
    total: u32,
    bytes: Vec<u8>,
    idle_retries: u8,
}

pub fn digest(text: &str) -> [u8; 32] {
    Sha256::digest(text.as_bytes()).into()
}

impl ClipboardSync {
    pub fn new(local: String, incarnation: String) -> Self {
        Self {
            local,
            incarnation,
            members: HashSet::new(),
            peers: HashMap::new(),
            session: None,
            router: None,
            latest: None,
            accepted: HashMap::new(),
            local_pending: None,
            cache: VecDeque::new(),
            images: VecDeque::new(),
            receiving: None,
            sequence: 0,
            serial: None,
            target: None,
            revision: 0,
            incoming_revision: 0,
            incoming: None,
            route_sent: None,
            fetch_sent: None,
            unavailable: None,
        }
    }

    fn send(device: String, message: Message) -> Action {
        Action::Send { device, message }
    }
    pub fn connected(&mut self, device: String, generation: u64) -> Vec<Action> {
        if self
            .peers
            .get(&device)
            .is_some_and(|(g, _)| *g == generation)
        {
            return vec![];
        }
        self.peers.insert(device.clone(), (generation, None));
        vec![Self::send(
            device,
            Message::Hello {
                incarnation: self.incarnation.clone(),
            },
        )]
    }
    pub fn disconnected(&mut self, device: &str) {
        self.peers.remove(device);
        if let Some(r) = &mut self.router {
            r.disconnect_source(device);
        }
        self.incoming = None;
        self.fetch_sent = None;
        self.receiving = None;
    }
    pub fn pause(&mut self) -> Vec<Action> {
        self.session = None;
        self.router = None;
        self.incoming = None;
        self.target = None;
        self.route_sent = None;
        self.fetch_sent = None;
        self.receiving = None;
        vec![Action::CancelImages, Action::Status(Status::Paused)]
    }
    pub fn start(&mut self, session: SessionId, members: HashSet<String>) -> Vec<Action> {
        if self.session.as_ref() == Some(&session) {
            return vec![];
        }
        self.members = members;
        self.session = Some(session.clone());
        self.incoming = None;
        self.incoming_revision = 0;
        self.revision = 0;
        self.route_sent = None;
        self.fetch_sent = None;
        self.receiving = None;
        let mut actions = vec![];
        if session.primary == self.local {
            let mut router = ClipboardRouter::new(session.clone(), self.members.iter().cloned())
                .expect("negotiated group");
            router
                .connect_source(&self.local, &self.incarnation)
                .expect("local member");
            for (device, (_, incarnation)) in &self.peers {
                if let Some(incarnation) = incarnation {
                    let _ = router.connect_source(device, incarnation);
                }
            }
            let latest = self
                .latest
                .as_ref()
                .filter(|(o, _)| self.members.contains(&o.copy.source))
                .map(|(o, _)| o.clone());
            let _ = router.resume_metadata(
                latest,
                self.accepted
                    .values()
                    .filter(|c| self.members.contains(&c.source))
                    .cloned(),
            );
            self.router = Some(router);
            if let Some(offer) = self.router.as_ref().and_then(|r| r.latest()).cloned() {
                let supported = self
                    .latest
                    .as_ref()
                    .is_some_and(|(_, supported)| *supported);
                actions.extend(self.publish_latest(offer, supported));
            }
            self.target = Some(self.local.clone());
            for device in &self.members {
                if *device != self.local {
                    actions.push(Self::send(
                        device.clone(),
                        Message::RequestMetadata {
                            session: session.clone(),
                        },
                    ));
                }
            }
        } else {
            self.router = None;
            actions.push(Self::send(
                session.primary.clone(),
                Message::RequestLatest { session },
            ));
        }
        actions.extend(self.announce_pending());
        actions.extend(self.drive());
        actions
    }

    pub fn target(&mut self, target: Option<String>) -> Vec<Action> {
        if self.target != target {
            self.target = target;
            self.fetch_sent = None;
        }
        self.drive()
    }

    /// The first read establishes a baseline, never publishes pre-existing
    /// user clipboard content merely because the application started.
    pub fn observe(&mut self, serial: u64, text: Option<String>) -> Vec<Action> {
        self.observe_contents(serial, text, None)
    }
    pub fn observe_contents(
        &mut self,
        serial: u64,
        text: Option<String>,
        image: Option<Image>,
    ) -> Vec<Action> {
        let previous = self.serial.replace(serial);
        if previous.is_none() || previous == Some(serial) {
            return vec![];
        }
        self.incoming = None;
        self.receiving = None;
        let Some(sequence) = self.sequence.checked_add(1) else {
            return vec![Action::Status(Status::Failed)];
        };
        self.sequence = sequence;
        let text = text.filter(|t| t.len() <= MAX_TEXT);
        let image = image.filter(Image::valid);
        let copy = CopyId {
            source: self.local.clone(),
            incarnation: self.incarnation.clone(),
            sequence,
        };
        let metadata = Metadata {
            copy: copy.clone(),
            digest: image
                .as_ref()
                .map(Image::digest)
                .or_else(|| text.as_deref().map(digest))
                .unwrap_or([0; 32]),
            supported: text.is_some() || image.is_some(),
            native_serial: serial,
        };
        if let Some(image) = image {
            self.images.push_back((copy.clone(), image));
        }
        self.cache.push_back((copy, text));
        while self.cache.len() > 8 {
            self.cache.pop_front();
        }
        while self.images.len() > 8
            || self
                .images
                .iter()
                .map(|(_, i)| i.bytes.len())
                .sum::<usize>()
                > 64 * 1024 * 1024
        {
            self.images.pop_front();
        }
        self.local_pending = Some(metadata);
        let mut actions = vec![Action::CancelImages];
        actions.extend(self.announce_pending());
        actions
    }
    fn announce_pending(&mut self) -> Vec<Action> {
        let (Some(session), Some(metadata)) = (self.session.clone(), self.local_pending.clone())
        else {
            return vec![];
        };
        if session.primary == self.local {
            self.accept(&self.local.clone(), metadata)
        } else {
            vec![Self::send(
                session.primary.clone(),
                Message::Announce { session, metadata },
            )]
        }
    }
    fn accept(&mut self, source: &str, metadata: Metadata) -> Vec<Action> {
        if self.accepted.get(source).is_some_and(|c| {
            c.incarnation == metadata.copy.incarnation && c.sequence >= metadata.copy.sequence
        }) {
            return vec![];
        }
        let Some(router) = &mut self.router else {
            return vec![];
        };
        if source != metadata.copy.source {
            return vec![];
        }
        match router.announce(source, metadata.copy.clone(), metadata.digest) {
            Ok(Some(_)) => {
                self.receiving = None;
                let offer = router.latest().expect("accepted offer").clone();
                self.fetch_sent = None;
                let mut actions = self.publish_latest(offer, metadata.supported);
                actions.extend(self.drive());
                actions
            }
            _ => vec![],
        }
    }
    fn publish_latest(&mut self, offer: Offer, supported: bool) -> Vec<Action> {
        self.record_latest(offer.clone(), supported);
        self.members
            .iter()
            .filter(|d| **d != self.local)
            .map(|device| {
                Self::send(
                    device.clone(),
                    Message::Latest {
                        offer: offer.clone(),
                        supported,
                    },
                )
            })
            .collect()
    }
    fn record_latest(&mut self, offer: Offer, supported: bool) {
        self.accepted
            .insert(offer.copy.source.clone(), offer.copy.clone());
        if self
            .local_pending
            .as_ref()
            .is_some_and(|m| m.copy == offer.copy)
        {
            self.local_pending = None;
        }
        self.latest = Some((offer, supported));
    }
    pub fn retry(&mut self) -> Vec<Action> {
        if let Some(assembly) = &mut self.receiving {
            assembly.idle_retries += 1;
            if assembly.idle_retries < 2 {
                return self.drive();
            }
            self.receiving = None;
        }
        self.fetch_sent = None;
        self.unavailable = None;
        let mut actions = self.announce_pending();
        if let Some(session) = &self.session {
            if session.primary != self.local {
                actions.push(Self::send(
                    session.primary.clone(),
                    Message::RequestLatest {
                        session: session.clone(),
                    },
                ));
            }
        }
        actions.extend(self.drive());
        actions
    }
    pub fn drive(&mut self) -> Vec<Action> {
        let Some(router) = &mut self.router else {
            return vec![];
        };
        let _ = router.set_target(self.target.clone());
        let route = match router.route() {
            Ok(r) => r,
            Err(_) => return vec![Action::Status(Status::Failed)],
        };
        let request = match &route {
            Route::Fetch(r) => Some(r.clone()),
            _ => None,
        };
        let mut actions = vec![];
        let status = match &route {
            Route::Idle => Status::Waiting,
            Route::Fetch(request) if self.unavailable.as_ref() == Some(request) => {
                Status::Unavailable
            }
            _ if self
                .latest
                .as_ref()
                .is_some_and(|(_, supported)| !supported) =>
            {
                Status::Unsupported
            }
            Route::Current => Status::Ready,
            Route::WaitingForCopy { .. } => Status::Syncing,
            Route::Unavailable { .. } => Status::Unavailable,
            Route::Fetch(_) => Status::Syncing,
        };
        let state = (self.target.clone(), request.clone(), status);
        if self.route_sent.as_ref() != Some(&state) {
            let Some(revision) = self.revision.checked_add(1) else {
                return vec![Action::Status(Status::Failed)];
            };
            self.revision = revision;
            self.route_sent = Some(state);
            self.receiving = None;
            actions.push(Action::CancelImages);
            for device in &self.members {
                if *device != self.local {
                    actions.push(Self::send(
                        device.clone(),
                        Message::Route {
                            session: self.session.clone().expect("router session"),
                            revision,
                            request: request.as_ref().filter(|r| r.target == *device).cloned(),
                            status,
                        },
                    ));
                }
            }
        }
        actions.push(Action::Status(status));
        if let Route::Fetch(request) = route {
            if status == Status::Syncing && self.fetch_sent.as_ref() != Some(&request) {
                self.fetch_sent = Some(request.clone());
                if request.offer.copy.source == self.local {
                    if let Some(image) = self.image(&request) {
                        actions.extend(self.deliver_image(request, image));
                    } else {
                        let text = self.body(&request);
                        actions.extend(self.deliver(request, text));
                    }
                } else {
                    actions.push(Self::send(
                        request.offer.copy.source.clone(),
                        Message::Fetch { request },
                    ));
                }
            }
        }
        actions
    }
    fn body(&self, request: &FetchRequest) -> Option<String> {
        self.cache
            .iter()
            .find(|(c, _)| c == &request.offer.copy)
            .and_then(|(_, t)| t.clone())
    }
    fn image(&self, request: &FetchRequest) -> Option<Image> {
        self.images
            .iter()
            .find(|(c, _)| c == &request.offer.copy)
            .map(|(_, i)| i.clone())
    }
    fn deliver_image(&self, request: FetchRequest, image: Image) -> Vec<Action> {
        if !self
            .router
            .as_ref()
            .is_some_and(|r| r.can_apply(&request, image.digest()))
        {
            return vec![];
        }
        if request.target == self.local {
            let Some(expected_serial) = self.serial else {
                return vec![];
            };
            if self.local_pending.is_some() {
                return vec![];
            }
            vec![Action::WriteImage {
                revision: self.revision,
                request,
                image,
                expected_serial,
            }]
        } else {
            vec![Action::SendImage {
                device: request.target.clone(),
                request,
                revision: Some(self.revision),
                image,
            }]
        }
    }
    pub fn can_write_image(&self, revision: u64, request: &FetchRequest, image: &Image) -> bool {
        image.valid()
            && image.digest() == request.offer.digest
            && self.write_permitted(revision, request)
    }
    fn write_permitted(&self, revision: u64, request: &FetchRequest) -> bool {
        self.serial.is_some()
            && self.local_pending.is_none()
            && self.session.as_ref() == Some(&request.offer.version.session)
            && request.target == self.local
            && (self.router.as_ref().is_some_and(|r| {
                self.revision == revision && r.can_apply(request, request.offer.digest)
            }) || self.router.is_none()
                && self.incoming_revision == revision
                && self.incoming.as_ref() == Some(request))
    }
    fn receive_image_part(
        &mut self,
        source: &str,
        request: FetchRequest,
        revision: Option<u64>,
        offset: u32,
        total: u32,
        bytes: Vec<u8>,
    ) -> Vec<Action> {
        let allowed = match revision {
            None => {
                source == request.offer.copy.source
                    && self
                        .router
                        .as_ref()
                        .is_some_and(|r| r.can_apply(&request, request.offer.digest))
            }
            Some(revision) => {
                source == request.offer.version.session.primary
                    && self.write_permitted(revision, &request)
            }
        };
        if !allowed {
            return vec![];
        }
        if offset == 0 {
            self.receiving = Some(Assembly {
                request: request.clone(),
                revision,
                total,
                bytes: Vec::new(),
                idle_retries: 0,
            });
        }
        let Some(assembly) = &mut self.receiving else {
            return vec![];
        };
        if assembly.request != request
            || assembly.revision != revision
            || assembly.total != total
            || assembly.bytes.len() != offset as usize
        {
            return vec![];
        }
        assembly.idle_retries = 0;
        assembly.bytes.extend_from_slice(&bytes);
        if assembly.bytes.len() != total as usize {
            return vec![];
        }
        let assembly = self.receiving.take().expect("complete image");
        let Ok(image) = Image::decode(&assembly.bytes) else {
            return vec![Action::Status(Status::Failed)];
        };
        if image.digest() != request.offer.digest {
            return vec![Action::Status(Status::Failed)];
        }
        match revision {
            None => self.deliver_image(request, image),
            Some(revision) => vec![Action::WriteImage {
                revision,
                request,
                image,
                expected_serial: self.serial.expect("native baseline before permission"),
            }],
        }
    }
    fn deliver(&mut self, request: FetchRequest, text: Option<String>) -> Vec<Action> {
        if !self
            .router
            .as_ref()
            .is_some_and(|r| r.can_apply(&request, request.offer.digest))
        {
            return vec![];
        }
        let Some(text) = text else {
            self.unavailable = Some(request);
            return self.drive();
        };
        if !self
            .router
            .as_ref()
            .is_some_and(|r| r.can_apply(&request, digest(&text)))
        {
            return vec![];
        }
        if request.target == self.local {
            self.write_action(self.revision, request, text)
        } else {
            vec![Self::send(
                request.target.clone(),
                Message::Apply {
                    revision: self.revision,
                    request,
                    text,
                },
            )]
        }
    }
    fn write_action(&self, revision: u64, request: FetchRequest, text: String) -> Vec<Action> {
        let Some(expected_serial) = self.serial else {
            return vec![Action::Status(Status::Waiting)];
        };
        if self.local_pending.is_some() {
            return vec![];
        }
        vec![Action::Write {
            revision,
            request,
            text,
            expected_serial,
        }]
    }
    pub fn can_write(&self, revision: u64, request: &FetchRequest, text: &str) -> bool {
        self.local_pending.is_none()
            && self.session.as_ref() == Some(&request.offer.version.session)
            && request.target == self.local
            && digest(text) == request.offer.digest
            && (self
                .router
                .as_ref()
                .is_some_and(|r| self.revision == revision && r.can_apply(request, digest(text)))
                || (self.router.is_none()
                    && self.incoming_revision == revision
                    && self.incoming.as_ref() == Some(request)))
    }
    pub fn written(&mut self, revision: u64, request: FetchRequest, serial: u64) -> Vec<Action> {
        self.serial = Some(serial);
        if let Some(router) = &mut self.router {
            if self.revision != revision || !router.applied(&request, request.offer.digest, serial)
            {
                return vec![];
            }
            self.drive()
        } else if self.incoming_revision == revision && self.incoming.as_ref() == Some(&request) {
            self.incoming = None;
            vec![
                Self::send(
                    request.offer.version.session.primary.clone(),
                    Message::Applied {
                        revision,
                        request,
                        native_serial: serial,
                    },
                ),
                Action::Status(Status::Ready),
            ]
        } else {
            vec![]
        }
    }

    pub fn receive(&mut self, source: &str, generation: u64, message: Message) -> Vec<Action> {
        if !message.validate()
            || !self
                .peers
                .get(source)
                .is_some_and(|(g, _)| *g == generation)
        {
            return vec![];
        }
        if let Message::Hello { incarnation } = message {
            let (_, registered) = self.peers.get_mut(source).expect("known connection");
            if registered.as_ref().is_some_and(|i| *i != incarnation) {
                return vec![];
            }
            *registered = Some(incarnation.clone());
            if let Some(router) = &mut self.router {
                let _ = router.connect_source(source, &incarnation);
                return vec![Self::send(
                    source.into(),
                    Message::RequestMetadata {
                        session: self.session.clone().expect("router session"),
                    },
                )];
            }
            return vec![];
        }
        let Some(session) = self.session.clone() else {
            return vec![];
        };
        match message {
            Message::RequestLatest { session: s }
                if s == session && self.router.is_some() && self.members.contains(source) =>
            {
                self.latest
                    .as_ref()
                    .map(|(offer, supported)| {
                        vec![Self::send(
                            source.into(),
                            Message::Latest {
                                offer: offer.clone(),
                                supported: *supported,
                            },
                        )]
                    })
                    .unwrap_or_default()
            }
            Message::Announce {
                session: s,
                metadata,
            } if s == session && self.router.is_some() => self.accept(source, metadata),
            Message::RequestMetadata { session: s }
                if s == session && source == session.primary =>
            {
                self.announce_pending()
            }
            Message::Latest { offer, supported }
                if source == session.primary && offer.version.session == session =>
            {
                if self.latest.as_ref().is_some_and(|(old, _)| {
                    old.version.session == session && old.version.sequence >= offer.version.sequence
                }) {
                    return vec![];
                }
                self.record_latest(offer, supported);
                vec![]
            }
            Message::Route {
                session: s,
                revision,
                request,
                status,
            } if source == session.primary && s == session && revision > self.incoming_revision => {
                if request
                    .as_ref()
                    .is_some_and(|r| r.target != self.local || r.offer.version.session != session)
                {
                    return vec![];
                }
                self.incoming_revision = revision;
                self.incoming = request;
                self.receiving = None;
                vec![Action::CancelImages, Action::Status(status)]
            }
            Message::Fetch { request }
                if source == session.primary
                    && request.offer.version.session == session
                    && request.offer.copy.source == self.local =>
            {
                if let Some(image) = self.image(&request) {
                    vec![Action::SendImage {
                        device: source.into(),
                        request,
                        revision: None,
                        image,
                    }]
                } else {
                    let text = self.body(&request);
                    vec![Self::send(source.into(), Message::Body { request, text })]
                }
            }
            Message::Body { request, text }
                if self.router.is_some()
                    && source == request.offer.copy.source
                    && request.offer.version.session == session =>
            {
                self.deliver(request, text)
            }
            Message::Apply {
                revision,
                request,
                text,
            } if source == session.primary && self.can_write(revision, &request, &text) => {
                self.write_action(revision, request, text)
            }
            Message::Applied {
                revision,
                request,
                native_serial,
            } if self.router.is_some() && source == request.target && revision == self.revision => {
                if self.router.as_mut().expect("primary").applied(
                    &request,
                    request.offer.digest,
                    native_serial,
                ) {
                    self.drive()
                } else {
                    vec![]
                }
            }
            Message::ImagePart {
                request,
                revision,
                offset,
                total,
                bytes,
            } => self.receive_image_part(source, request, revision, offset, total, bytes),
            _ => vec![],
        }
    }
}

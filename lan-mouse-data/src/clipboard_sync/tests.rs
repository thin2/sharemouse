use super::*;
use crate::protocol::{DataMessage, Frame};

#[test]
fn follower_starting_after_latest_recovers_before_primary_handover() {
    let mut cluster = Cluster::new();
    for node in &mut cluster.nodes {
        node.pause();
    }
    cluster.copy(2, Some("paused copy"));
    let session = SessionId {
        primary: name(2),
        nonce: "delayed-start".into(),
    };
    for i in [0, 2] {
        let actions = cluster.nodes[i].start(session.clone(), (0..3).map(name).collect());
        cluster.push(i, actions);
    }
    cluster.drain(); // b has no session and drops c's first Latest.
    let actions = cluster.nodes[1].start(session, (0..3).map(name).collect());
    cluster.push(1, actions);
    // Also exercise retry when the first snapshot request is lost.
    cluster.queue.clear();
    let actions = cluster.nodes[1].retry();
    cluster.push(1, actions);
    cluster.drain();
    assert_eq!(
        cluster.nodes[1].latest.as_ref().unwrap().0.copy.source,
        name(2)
    );
    for node in &mut cluster.nodes {
        node.pause();
    }
    cluster.start(1, "handover");
    cluster.target(1, 0);
    assert_eq!(cluster.boards[0].as_deref(), Some("paused copy"));
}

struct Cluster {
    nodes: Vec<ClipboardSync>,
    serials: Vec<u64>,
    boards: Vec<Option<String>>,
    image_boards: Vec<Option<Image>>,
    queue: VecDeque<(usize, Action)>,
    writes: Vec<(usize, u64, FetchRequest, String, u64)>,
    image_writes: Vec<(usize, u64, FetchRequest, Image, u64)>,
    bodies: Vec<(usize, usize)>,
    hold_writes: bool,
}
fn name(index: usize) -> String {
    ["a", "b", "c"][index].into()
}
// Wire tests deliberately use header-only payloads; native decoder tests use
// complete PNGs and verify pixel values separately.
fn wire_image(size: usize) -> Image {
    let mut bytes = vec![42; size.max(33)];
    bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    bytes[12..16].copy_from_slice(b"IHDR");
    bytes[16..20].copy_from_slice(&1_u32.to_be_bytes());
    bytes[20..24].copy_from_slice(&1_u32.to_be_bytes());
    Image::new(bytes, Some("透明图.png")).unwrap()
}
fn index(name: &str) -> usize {
    ["a", "b", "c"].iter().position(|n| *n == name).unwrap()
}
impl Cluster {
    fn new() -> Self {
        let mut c = Self {
            nodes: (0..3)
                .map(|i| ClipboardSync::new(name(i), format!("p{i}")))
                .collect(),
            serials: vec![1; 3],
            boards: vec![None; 3],
            image_boards: vec![None; 3],
            queue: VecDeque::new(),
            writes: vec![],
            image_writes: vec![],
            bodies: vec![],
            hold_writes: false,
        };
        for i in 0..3 {
            c.nodes[i].observe(1, None);
            for j in 0..3 {
                if i != j {
                    let actions = c.nodes[i].connected(name(j), 1);
                    c.push(i, actions);
                }
            }
        }
        c.drain();
        c.start(0, "s1");
        c
    }
    fn push(&mut self, i: usize, actions: Vec<Action>) {
        self.queue.extend(actions.into_iter().map(|a| (i, a)));
    }
    fn start(&mut self, primary: usize, nonce: &str) {
        // Start followers first, as control queues may have already delivered Start.
        for i in (0..3)
            .filter(|i| *i != primary)
            .chain(std::iter::once(primary))
        {
            let actions = self.nodes[i].start(
                SessionId {
                    primary: name(primary),
                    nonce: nonce.into(),
                },
                (0..3).map(name).collect(),
            );
            self.push(i, actions);
        }
        self.drain();
    }
    fn copy(&mut self, i: usize, text: Option<&str>) {
        self.image_boards[i] = None;
        self.serials[i] += 1;
        self.boards[i] = text.map(str::to_owned);
        let actions = self.nodes[i].observe(self.serials[i], self.boards[i].clone());
        self.push(i, actions);
        self.drain();
    }
    fn copy_image(&mut self, i: usize, image: Image) {
        self.serials[i] += 1;
        self.boards[i] = None;
        self.image_boards[i] = Some(image.clone());
        let actions = self.nodes[i].observe_contents(self.serials[i], None, Some(image));
        self.push(i, actions);
        self.drain();
    }
    fn commit_image(
        &mut self,
        i: usize,
        revision: u64,
        request: FetchRequest,
        image: Image,
        expected: u64,
    ) {
        if expected == self.serials[i] && self.nodes[i].can_write_image(revision, &request, &image)
        {
            self.serials[i] += 1;
            self.boards[i] = None;
            self.image_boards[i] = Some(image);
            let actions = self.nodes[i].written(revision, request, self.serials[i]);
            self.push(i, actions);
        }
    }
    fn target(&mut self, primary: usize, target: usize) {
        let actions = self.nodes[primary].target(Some(name(target)));
        self.push(primary, actions);
        self.drain();
    }
    fn commit(
        &mut self,
        i: usize,
        revision: u64,
        request: FetchRequest,
        text: String,
        expected: u64,
    ) {
        if expected == self.serials[i] && self.nodes[i].can_write(revision, &request, &text) {
            self.serials[i] += 1;
            self.boards[i] = Some(text);
            self.image_boards[i] = None;
            let actions = self.nodes[i].written(revision, request, self.serials[i]);
            self.push(i, actions);
        }
    }
    fn drain(&mut self) {
        let mut count = 0;
        while let Some((i, action)) = self.queue.pop_front() {
            count += 1;
            assert!(count < 2000, "clipboard loop");
            match action {
                Action::CancelImages => {}
                Action::SendImage {
                    device,
                    request,
                    revision,
                    image,
                } => {
                    let j = index(&device);
                    self.bodies.push((i, j));
                    let bytes = image.encode();
                    for (part, chunk) in bytes.chunks(PART_BYTES).enumerate() {
                        let message = Message::ImagePart {
                            request: request.clone(),
                            revision,
                            offset: (part * PART_BYTES) as u32,
                            total: bytes.len() as u32,
                            bytes: chunk.to_vec(),
                        };
                        let encoded = Frame::Control(DataMessage::Clipboard { message })
                            .encode()
                            .unwrap();
                        let Frame::Control(DataMessage::Clipboard { message }) =
                            Frame::decode(&encoded).unwrap()
                        else {
                            panic!()
                        };
                        let actions = self.nodes[j].receive(&name(i), 1, message);
                        self.push(j, actions);
                    }
                }
                Action::WriteImage {
                    revision,
                    request,
                    image,
                    expected_serial,
                } => {
                    if self.hold_writes {
                        self.image_writes
                            .push((i, revision, request, image, expected_serial));
                    } else {
                        self.commit_image(i, revision, request, image, expected_serial);
                    }
                }
                Action::Send { device, message } => {
                    let j = index(&device);
                    if matches!(message, Message::Apply { .. } | Message::Body { .. }) {
                        self.bodies.push((i, j));
                    }
                    let frame = Frame::Control(DataMessage::Clipboard { message });
                    let bytes = frame.encode().unwrap();
                    let Frame::Control(DataMessage::Clipboard { message }) =
                        Frame::decode(&bytes).unwrap()
                    else {
                        panic!()
                    };
                    let actions = self.nodes[j].receive(&name(i), 1, message);
                    self.push(j, actions);
                }
                Action::Write {
                    revision,
                    request,
                    text,
                    expected_serial,
                } => {
                    if self.hold_writes {
                        self.writes
                            .push((i, revision, request, text, expected_serial));
                    } else {
                        self.commit(i, revision, request, text, expected_serial);
                    }
                }
                Action::Status(_) => {}
            }
        }
    }
    fn release_writes(&mut self) {
        self.hold_writes = false;
        for (i, revision, request, text, expected) in std::mem::take(&mut self.writes) {
            self.commit(i, revision, request, text, expected);
        }
        for (i, revision, request, image, expected) in std::mem::take(&mut self.image_writes) {
            self.commit_image(i, revision, request, image, expected);
        }
        self.drain();
    }
}

#[test]
fn image_chunks_follow_target_and_keep_original_bytes_without_echo() {
    let mut c = Cluster::new();
    let image = wire_image(1024 * 1024 + 7);
    c.copy_image(0, image.clone());
    assert!(c.bodies.is_empty());
    c.target(0, 1);
    assert_eq!(c.image_boards[1], Some(image.clone()));
    assert!(c.image_boards[2].is_none());
    c.target(0, 2);
    assert_eq!(c.image_boards[2], Some(image));
    assert_eq!(c.bodies, [(0, 1), (0, 2)]);
    for i in 0..3 {
        assert!(
            c.nodes[i]
                .observe_contents(c.serials[i], None, c.image_boards[i].clone())
                .is_empty()
        );
    }
    assert_eq!(c.nodes[1].sequence, 0);
    assert_eq!(c.nodes[2].sequence, 0);
    // A follower's genuine copy routes through the primary to the current target.
    let second = wire_image(100);
    c.copy_image(1, second.clone());
    assert_eq!(c.image_boards[2], Some(second));
    assert_eq!(&c.bodies[2..], [(1, 0), (0, 2)]);
}

#[test]
fn delayed_image_preparation_cannot_overwrite_new_target_copy_or_session() {
    let mut c = Cluster::new();
    c.copy_image(0, wire_image(120_000));
    c.hold_writes = true;
    c.target(0, 1);
    assert_eq!(c.image_writes.len(), 1);
    c.target(0, 2);
    c.release_writes();
    assert!(c.image_boards[1].is_none());
    assert!(c.image_boards[2].is_some());

    c.hold_writes = true;
    c.copy_image(0, wire_image(100));
    c.copy(2, Some("local copy during image preparation"));
    c.release_writes();
    assert_eq!(
        c.boards[2].as_deref(),
        Some("local copy during image preparation")
    );
    c.hold_writes = true;
    c.copy_image(0, wire_image(101));
    let actions = c.nodes[2].pause();
    c.push(2, actions);
    c.release_writes();
    assert_eq!(
        c.boards[2].as_deref(),
        Some("local copy during image preparation")
    );
}

#[test]
fn image_chunk_authentication_order_digest_bounds_and_native_baseline() {
    let mut c = Cluster::new();
    let image = wire_image(100_000);
    c.copy_image(0, image.clone());
    c.hold_writes = true;
    c.target(0, 1);
    let (_, revision, request, _, _) = c.image_writes.pop().unwrap();
    let mut bytes = image.encode();
    let message = |offset: usize, bytes: Vec<u8>, total: usize| Message::ImagePart {
        request: request.clone(),
        revision: Some(revision),
        offset: offset as u32,
        total: total as u32,
        bytes,
    };
    assert!(
        c.nodes[1]
            .receive("c", 1, message(0, bytes[..10].to_vec(), bytes.len()))
            .is_empty()
    );
    assert!(c.nodes[1].receiving.is_none());
    assert!(
        c.nodes[1]
            .receive("a", 2, message(0, bytes[..10].to_vec(), bytes.len()))
            .is_empty()
    );
    assert!(
        c.nodes[1]
            .receive("a", 1, message(20, bytes[20..30].to_vec(), bytes.len()))
            .is_empty()
    );
    assert!(c.nodes[1].receiving.is_none());
    assert!(!message(0, vec![1; PART_BYTES + 1], MAX_ENCODED).validate());
    assert!(!message(0, vec![1], MAX_ENCODED + 1).validate());
    // A complete but tampered body never reaches a native write.
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    let mut actions = vec![];
    for (n, part) in bytes.chunks(PART_BYTES).enumerate() {
        actions.extend(c.nodes[1].receive(
            "a",
            1,
            message(n * PART_BYTES, part.to_vec(), bytes.len()),
        ));
    }
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, Action::Status(Status::Failed)))
    );
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, Action::WriteImage { .. }))
    );
    c.nodes[1].serial = None;
    assert!(
        c.nodes[1]
            .receive("a", 1, message(0, vec![1], 1))
            .is_empty()
    );
    assert!(c.nodes[1].receiving.is_none());
}

#[test]
fn copy_follows_a_b_c_without_body_broadcast_or_echo() {
    let mut c = Cluster::new();
    c.copy(0, Some("中文 🦀\nsecond line"));
    assert!(c.bodies.is_empty());
    assert!(c.boards[1].is_none() && c.boards[2].is_none());
    c.target(0, 1);
    assert_eq!(c.boards[1], c.boards[0]);
    assert!(c.boards[2].is_none());
    c.target(0, 2);
    assert_eq!(c.boards[2], c.boards[0]);
    let version = c.nodes[0].latest.clone();
    for i in 0..3 {
        assert!(
            c.nodes[i]
                .observe(c.serials[i], c.boards[i].clone())
                .is_empty()
        );
    }
    assert_eq!(c.nodes[0].latest, version);
    assert_eq!(c.nodes[1].sequence, 0);
    assert_eq!(c.nodes[2].sequence, 0);
    assert_eq!(c.bodies, vec![(0, 1), (0, 2)]);
}

#[test]
fn any_source_and_same_bytes_new_copy_supersede_previous_version() {
    let mut c = Cluster::new();
    c.copy(0, Some("a"));
    c.target(0, 1);
    c.copy(1, Some("b"));
    c.target(0, 2);
    assert_eq!(c.boards[2].as_deref(), Some("b"));
    let old = c.nodes[0].latest.clone().unwrap().0;
    c.copy(1, Some("b"));
    let new = &c.nodes[0].latest.as_ref().unwrap().0;
    assert!(new.version.sequence > old.version.sequence);
    assert!(new.copy.sequence > old.copy.sequence);
}

#[test]
fn target_away_and_back_rejects_delayed_write() {
    let mut c = Cluster::new();
    c.copy(0, Some("x"));
    c.hold_writes = true;
    c.target(0, 1);
    c.target(0, 2);
    c.target(0, 1);
    assert_eq!(c.writes.len(), 3);
    let stale = &c.writes[0];
    assert!(!c.nodes[1].can_write(stale.1, &stale.2, &stale.3));
    c.release_writes();
    assert_eq!(c.boards[1].as_deref(), Some("x"));
    assert!(c.boards[2].is_none());
}

#[test]
fn local_copy_before_final_write_cancels_old_download() {
    let mut c = Cluster::new();
    c.copy(0, Some("old"));
    c.hold_writes = true;
    c.target(0, 1);
    c.copy(1, Some("new"));
    c.release_writes();
    assert_eq!(c.boards[1].as_deref(), Some("new"));
    c.target(0, 2);
    assert_eq!(c.boards[2].as_deref(), Some("new"));
}

#[test]
fn stop_cancels_write_and_handover_rebases_latest() {
    let mut c = Cluster::new();
    c.copy(0, Some("kept"));
    c.hold_writes = true;
    c.target(0, 1);
    for n in &mut c.nodes {
        n.pause();
    }
    c.release_writes();
    assert!(c.boards[1].is_none());
    c.start(1, "s2");
    assert_eq!(c.boards[1].as_deref(), Some("kept"));
    c.target(1, 2);
    assert_eq!(c.boards[2].as_deref(), Some("kept"));
    assert_eq!(
        c.nodes[1].latest.as_ref().unwrap().0.version.session.nonce,
        "s2"
    );
}

#[test]
fn unsupported_copy_blocks_stale_text_and_duplicate_does_not_invalidate_route() {
    let mut c = Cluster::new();
    c.copy(0, Some("old"));
    c.target(0, 1);
    c.copy(2, None);
    assert_eq!(c.boards[1].as_deref(), Some("old"));
    let offer = c.nodes[0].latest.clone().unwrap().0;
    let metadata = Metadata {
        copy: offer.copy,
        digest: offer.digest,
        supported: false,
        native_serial: 2,
    };
    let session = c.nodes[0].session.clone().unwrap();
    assert!(
        c.nodes[0]
            .receive("c", 1, Message::Announce { session, metadata })
            .is_empty()
    );
    assert!(
        c.nodes[0]
            .drive()
            .iter()
            .any(|a| matches!(a, Action::Status(Status::Unsupported)))
    );
}

#[test]
fn old_connection_and_old_source_incarnation_cannot_announce() {
    let mut c = Cluster::new();
    c.nodes[0].connected("b".into(), 2);
    assert!(
        c.nodes[0]
            .receive(
                "b",
                1,
                Message::Hello {
                    incarnation: "old".into()
                }
            )
            .is_empty()
    );
    c.nodes[0].receive(
        "b",
        2,
        Message::Hello {
            incarnation: "new".into(),
        },
    );
    let session = c.nodes[0].session.clone().unwrap();
    let actions = c.nodes[0].receive(
        "b",
        2,
        Message::Announce {
            session,
            metadata: Metadata {
                copy: CopyId {
                    source: "b".into(),
                    incarnation: "p1".into(),
                    sequence: 1,
                },
                digest: digest("old"),
                supported: true,
                native_serial: 2,
            },
        },
    );
    assert!(actions.is_empty());
    assert!(c.nodes[0].latest.is_none());
}

#[test]
fn copy_while_paused_is_announced_after_start() {
    let mut c = Cluster::new();
    for n in &mut c.nodes {
        n.pause();
    }
    c.copy(2, Some("during stop"));
    c.start(1, "s2");
    assert_eq!(c.boards[1].as_deref(), Some("during stop"));
    c.target(1, 0);
    assert_eq!(c.boards[0].as_deref(), Some("during stop"));
}

#[test]
fn retry_recovers_metadata_that_arrived_before_primary_start() {
    let mut c = Cluster::new();
    for n in &mut c.nodes {
        n.pause();
    }
    c.copy(2, Some("pending"));
    let session = SessionId {
        primary: "a".into(),
        nonce: "race".into(),
    };
    let actions = c.nodes[2].start(session.clone(), (0..3).map(name).collect());
    c.push(2, actions);
    c.drain();
    c.nodes[0].start(session, (0..3).map(name).collect());
    let actions = c.nodes[2].retry();
    c.push(2, actions);
    c.drain();
    assert_eq!(c.boards[0].as_deref(), Some("pending"));
}

#[test]
fn maximum_escaped_text_fits_frame_and_oversized_copy_is_unsupported() {
    let mut c = Cluster::new();
    let text = "\u{1}".repeat(MAX_TEXT);
    c.copy(0, Some(&text));
    c.target(0, 1);
    assert_eq!(c.boards[1].as_deref(), Some(text.as_str()));
    c.copy(0, Some(&"a".repeat(MAX_TEXT + 1)));
    assert!(!c.nodes[0].latest.as_ref().unwrap().1);
}

#[test]
fn missing_body_reports_unavailable_and_can_be_retried() {
    let mut c = Cluster::new();
    c.copy(0, Some("cached"));
    let cached = c.nodes[0].cache.pop_back().unwrap();
    c.target(0, 1);
    assert!(c.boards[1].is_none());
    assert!(
        c.nodes[0]
            .drive()
            .iter()
            .any(|a| matches!(a, Action::Status(Status::Unavailable)))
    );
    c.nodes[0].cache.push_back(cached);
    let actions = c.nodes[0].retry();
    c.push(0, actions);
    c.drain();
    assert_eq!(c.boards[1].as_deref(), Some("cached"));
}

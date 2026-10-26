//! In-memory record of manual text and link messages.
//!
//! A17 asks that a manually sent message appears in the message record
//! and does not touch the system clipboard. A18 asks that the record is
//! gone after a real restart. Both follow from keeping the log in
//! memory only: nothing here is written to disk, and [`MessageLog`] is
//! dropped with the process.
//!
//! The log deliberately does not persist even when the window is
//! closed to the menu bar (A19). Closing the window is not quitting,
//! and a user who reopens it expects their messages to still be there.

use std::collections::VecDeque;

/// How many messages are kept per conversation.
///
/// Bounds memory in a long-running tray app. Messages are not persisted.
pub const MAX_MESSAGES: usize = 500;

/// Which way a message travelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// This device wrote it.
    Sent,
    /// A peer wrote it.
    Received,
}

/// One message in the record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub direction: Direction,
    /// Stable identity used for conversation grouping, independent of display name.
    pub peer_id: String,
    /// Display label for the other device at the time the message was
    /// recorded.
    ///
    /// Shown to the user; grouping uses peer_id so identical names and
    /// later renames cannot mix messages from different devices.
    pub peer: String,
    pub text: String,
    /// Wire id, so a future acknowledgement can be matched up.
    pub id: String,
    pub image: Option<(std::path::PathBuf, std::path::PathBuf)>,
}

impl Message {
    /// Whether the text looks like a single link.
    ///
    /// Only used for presentation. Deliberately strict: a message
    /// containing prose must not lose its text because one word looks
    /// like a URL.
    pub fn is_link(&self) -> bool {
        if self.image.is_some() {
            return false;
        }
        let text = self.text.trim();
        if text.is_empty() || text.split_whitespace().count() != 1 {
            return false;
        }
        let Some((scheme, rest)) = text.split_once("://") else {
            return false;
        };
        if rest.is_empty() || !scheme.bytes().all(|b| b.is_ascii_alphabetic()) {
            return false;
        }
        matches!(
            scheme.to_ascii_lowercase().as_str(),
            "http" | "https" | "file" | "ftp" | "ftps" | "ssh" | "mailto" | "smb"
        )
    }
}

/// All manual messages for this run.
#[derive(Debug, Default)]
pub struct MessageLog {
    messages: VecDeque<Message>,
    /// Messages older than this many were dropped.
    dropped: usize,
}

impl MessageLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a message, dropping the oldest when the cap is reached.
    pub fn push(&mut self, message: Message) {
        if self.messages.len() >= MAX_MESSAGES {
            self.messages.pop_front();
            self.dropped += 1;
        }
        self.messages.push_back(message);
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Message> {
        self.messages.iter()
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// How many messages the cap has discarded.
    pub fn dropped(&self) -> usize {
        self.dropped
    }

    /// Remove everything, for the "clear record" action.
    pub fn clear(&mut self) {
        self.messages.clear();
        self.dropped = 0;
    }

    /// Messages with one peer, oldest first.
    pub fn with_peer<'a>(&'a self, peer: &'a str) -> impl DoubleEndedIterator<Item = &'a Message> {
        self.messages.iter().filter(move |m| m.peer_id == peer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(peer: &str, text: &str, id: &str) -> Message {
        Message {
            peer_id: peer.to_owned(),
            image: None,
            direction: Direction::Received,
            peer: peer.to_owned(),
            text: text.to_owned(),
            id: id.to_owned(),
        }
    }

    #[test]
    fn messages_are_kept_in_arrival_order() {
        let mut log = MessageLog::new();
        log.push(message("a", "first", "1"));
        log.push(message("a", "second", "2"));
        let texts: Vec<&str> = log.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, vec!["first", "second"]);
    }

    #[test]
    fn the_cap_drops_the_oldest_message_first() {
        let mut log = MessageLog::new();
        for i in 0..MAX_MESSAGES + 3 {
            log.push(message("a", &format!("m{i}"), &i.to_string()));
        }
        assert_eq!(log.len(), MAX_MESSAGES);
        assert_eq!(log.dropped(), 3);
        // The three oldest are gone, the newest is present.
        assert_eq!(log.iter().next().unwrap().text, "m3");
        assert_eq!(
            log.iter().last().unwrap().text,
            format!("m{}", MAX_MESSAGES + 2)
        );
    }

    #[test]
    fn clearing_removes_every_record() {
        let mut log = MessageLog::new();
        log.push(message("a", "hello", "1"));
        log.clear();
        assert!(log.is_empty());
        assert_eq!(log.dropped(), 0);
    }

    #[test]
    fn messages_are_filtered_by_peer() {
        let mut log = MessageLog::new();
        log.push(message("a", "from a", "1"));
        log.push(message("b", "from b", "2"));
        let texts: Vec<&str> = log.with_peer("a").map(|m| m.text.as_str()).collect();
        assert_eq!(texts, vec!["from a"]);
    }

    #[test]
    fn identical_names_and_renames_do_not_mix_conversations() {
        let mut log = MessageLog::new();
        let mut first = message("identity-a", "from a", "1");
        first.peer = "Desktop".into();
        let mut second = message("identity-b", "from b", "2");
        second.peer = "Desktop".into();
        let mut renamed = message("identity-a", "still a", "3");
        renamed.peer = "Renamed desktop".into();
        log.push(first);
        log.push(second);
        log.push(renamed);
        assert_eq!(log.with_peer("identity-a").count(), 2);
        assert_eq!(log.with_peer("identity-b").count(), 1);
    }

    #[test]
    fn sent_and_received_are_distinguished() {
        let mut log = MessageLog::new();
        log.push(Message {
            peer_id: "0".into(),
            image: None,
            direction: Direction::Sent,
            peer: "0".into(),
            text: "mine".into(),
            id: "1".into(),
        });
        log.push(message("0", "theirs", "2"));
        let directions: Vec<Direction> = log.iter().map(|m| m.direction).collect();
        assert_eq!(directions, vec![Direction::Sent, Direction::Received]);
    }

    #[test]
    fn wire_counters_reused_by_directions_or_restarted_peers_do_not_lose_messages() {
        let mut log = MessageLog::new();
        log.push(message("a", "hello", "id-1"));
        let mut sent = message("a", "reply", "id-1");
        sent.direction = Direction::Sent;
        log.push(sent);
        log.push(message("a", "after restart", "id-1"));
        assert_eq!(log.len(), 3);
    }

    #[test]
    fn a_bare_link_is_recognised() {
        for text in [
            "https://example.com",
            "http://example.com/a?b=c",
            "  https://example.com  ",
            "file:///tmp/a.txt",
        ] {
            let m = message("a", text, "1");
            assert!(m.is_link(), "{text} should be a link");
        }
    }

    #[test]
    fn prose_mentioning_a_link_is_not_treated_as_one() {
        // Losing the sentence because it contains a URL would be worse
        // than not offering the shortcut.
        let m = message("a", "see https://example.com for details", "1");
        assert!(!m.is_link());
    }

    #[test]
    fn non_links_and_bare_domains_are_not_links() {
        for text in [
            "hello",
            "example.com",
            "javascript:alert(1)",
            "://missing-scheme",
            "https://",
            "",
        ] {
            let m = message("a", text, "1");
            assert!(!m.is_link(), "{text} should not be a link");
        }
    }

    #[test]
    fn clearing_is_the_only_way_to_lose_messages_besides_the_cap() {
        let mut log = MessageLog::new();
        log.push(message("a", "hello", "1"));
        // Reading the log must not consume or mutate it.
        assert_eq!(log.iter().count(), 1);
        assert_eq!(log.iter().count(), 1);
        log.clear();
        assert!(log.is_empty());
    }
}

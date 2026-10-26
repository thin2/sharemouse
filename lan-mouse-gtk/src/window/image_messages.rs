use super::*;

impl Window {
    pub(crate) fn record_image_message(
        &self,
        device: &str,
        outgoing: Option<ClientHandle>,
        id: &str,
        filename: &str,
        preview: std::path::PathBuf,
        thumbnail: std::path::PathBuf,
    ) {
        let (peer, direction) = outgoing
            .map(|h| (self.device_name(h), Direction::Sent))
            .unwrap_or_else(|| {
                (
                    self.device_name_for_fingerprint(device),
                    Direction::Received,
                )
            });
        self.imp().messages.borrow_mut().push(Message {
            peer_id: device.to_owned(),
            direction,
            peer,
            text: filename.to_owned(),
            id: id.to_owned(),
            image: Some((preview, thumbnail)),
        });
        self.conversation_message_arrived(device, direction);
    }
}

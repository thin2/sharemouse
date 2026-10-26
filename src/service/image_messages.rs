use super::*;
use crate::message_images::{Command as ImageCommand, Event as ImageEvent};
use lan_mouse_data::{
    clipboard_image::Image,
    image_message::{Part, Receiver},
};

pub(super) struct IncomingImage {
    generation: u64,
    epoch: u64,
    pub updated: std::time::Instant,
    receiver: Receiver,
}
impl IncomingImage {
    fn push(
        &mut self,
        generation: u64,
        epoch: u64,
        part: Part,
    ) -> io::Result<Option<(String, Image)>> {
        if self.generation != generation {
            self.generation = generation;
            self.receiver = Receiver::default();
        }
        if part.offset == 0 {
            self.epoch = epoch;
        }
        self.updated = std::time::Instant::now();
        let completed = self.receiver.push(part)?;
        Ok(completed.filter(|_| self.epoch == epoch))
    }
}
impl Service {
    pub(super) fn image_message_command(&mut self, command: ImageCommand) {
        if self.message_images.tx.try_send(command).is_err() {
            self.notify_frontend(FrontendEvent::Error("Image message operation failed".into()));
        }
    }
    pub(super) fn handle_image_message_event(&mut self, event: ImageEvent) {
        match event {
            ImageEvent::Error => {
                self.notify_frontend(FrontendEvent::Error("Image message operation failed".into()));
            }
            ImageEvent::Message {
                device,
                generation,
                id,
                filename,
                preview,
                thumbnail,
                epoch,
            } => {
                if epoch != self.message_epoch
                    || self.data_sessions.generation(&device) != Some(generation)
                    || !self
                        .allowed_fingerprints
                        .read()
                        .expect("lock")
                        .contains(&device)
                {
                    return;
                }
                self.notify_frontend(FrontendEvent::ImageMessage {
                    device,
                    outgoing: None,
                    message_id: id,
                    filename,
                    preview,
                    thumbnail,
                });
            }
        }
    }
    pub(super) fn receive_image_message(&mut self, device: String, generation: u64, part: Part) {
        if self.data_sessions.generation(&device) != Some(generation)
            || !self
                .allowed_fingerprints
                .read()
                .expect("lock")
                .contains(&device)
        {
            return;
        }
        if !self.image_messages.contains_key(&device) && self.image_messages.len() >= 2 {
            self.notify_frontend(FrontendEvent::Error("Too many incoming image messages".into()));
            return;
        }
        let entry = self
            .image_messages
            .entry(device.clone())
            .or_insert_with(|| IncomingImage {
                generation,
                epoch: self.message_epoch,
                updated: std::time::Instant::now(),
                receiver: Receiver::default(),
            });
        match entry.push(generation, self.message_epoch, part) {
            Ok(Some((id, image))) => {
                self.image_messages.remove(&device);
                self.image_message_command(ImageCommand::Message {
                    device,
                    generation,
                    id,
                    image,
                    epoch: self.message_epoch,
                });
            }
            Ok(None) => {}
            Err(_) => {
                self.image_messages.remove(&device);
                self.notify_frontend(FrontendEvent::Error("Invalid image message".into()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clearing_during_image_assembly_discards_old_completion() {
        let image =
            Image::new(b"GIF89a\x01\0\x01\0\x80\0\0".to_vec(), Some("fixture.gif")).unwrap();
        let bytes = image.encode();
        let split = bytes.len() / 2;
        let part = |offset: usize, data: &[u8]| Part {
            id: "fixture".into(),
            digest: image.digest(),
            offset: offset as u32,
            total: bytes.len() as u32,
            bytes: data.into(),
        };
        let mut incoming = IncomingImage {
            generation: 1,
            epoch: 0,
            updated: std::time::Instant::now(),
            receiver: Receiver::default(),
        };
        assert!(incoming.push(1, 0, part(0, &bytes[..split])).unwrap().is_none());
        assert!(incoming.push(1, 1, part(split, &bytes[split..])).unwrap().is_none());
        assert!(incoming.push(1, 1, part(0, &bytes[..split])).unwrap().is_none());
        assert_eq!(
            incoming.push(1, 1, part(split, &bytes[split..])).unwrap(),
            Some(("fixture".into(), image.clone()))
        );
        assert!(incoming.push(1, 1, part(0, &bytes[..split])).unwrap().is_none());
        assert!(incoming.push(2, 1, part(split, &bytes[split..])).is_err());
    }
}

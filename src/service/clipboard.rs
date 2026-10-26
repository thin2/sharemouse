use super::*;
use lan_mouse_data::clipboard_sync::{Action, Status as ClipboardState};

impl Service {
    /// Input control and clipboard messages share one generation-bound writer.
    pub(super) fn send_session_control(&mut self, device: &str, message: DataMessage) -> bool {
        self.queue_session_control(device, SessionControl::Message(message))
    }
    fn queue_session_control(&mut self, device: &str, control: SessionControl) -> bool {
        let Some(generation) = self.data_sessions.generation(device) else {
            return false;
        };
        if !self.sharing_outgoing.contains_key(device) {
            let sessions = self.data_sessions.clone();
            let (tx, mut rx) = tokio::sync::mpsc::channel::<SessionControl>(64);
            let pending_image = std::rc::Rc::new(std::cell::RefCell::new(None));
            self.sharing_outgoing.insert(
                device.into(),
                SessionSender {
                    control: tx,
                    image: pending_image.clone(),
                },
            );
            let peer = device.to_owned();
            let wire_epoch = self.clipboard_wire_epoch.clone();
            self.sharing_jobs.spawn_local(async move {
                let mut result = Ok(());
                let mut image = None;
                loop {
                    // Process control messages between image chunks. A route
                    // change can cancel an old stream without waiting for it.
                    let next = match rx.try_recv() {
                        Ok(control) => Some(control),
                        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
                        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                            let replacement = pending_image.borrow_mut().take();
                            match replacement.or_else(|| image.take()) {
                                Some(stream) => Some(stream),
                                None => rx.recv().await,
                            }
                        }
                    };
                    let Some(control) = next else {
                        break;
                    };
                    if sessions.generation(&peer) != Some(generation) {
                        break;
                    }
                    let message = match control {
                        SessionControl::Message(message) => message,
                        SessionControl::ImageReady => continue,
                        SessionControl::Image {
                            request,
                            revision,
                            bytes,
                            offset,
                            epoch,
                        } => {
                            if epoch != wire_epoch.get() {
                                continue;
                            }
                            if image.as_ref().is_some_and(|pending| {
                                matches!(pending,
                                SessionControl::Image { request:r,revision:v,epoch:e,.. }
                                if *r == request && *v == revision && *e == epoch)
                            }) {
                                continue;
                            }
                            image = None;
                            let end = (offset + lan_mouse_data::clipboard_image::PART_BYTES)
                                .min(bytes.len());
                            let message = DataMessage::Clipboard {
                                message: lan_mouse_data::clipboard_sync::Message::ImagePart {
                                    request: request.clone(),
                                    revision,
                                    offset: offset as u32,
                                    total: bytes.len() as u32,
                                    bytes: bytes[offset..end].to_vec(),
                                },
                            };
                            if end < bytes.len() {
                                image = Some(SessionControl::Image {
                                    request,
                                    revision,
                                    bytes,
                                    offset: end,
                                    epoch,
                                });
                            }
                            message
                        }
                    };
                    result = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        sessions.send(&peer, message),
                    )
                    .await
                    .map_err(|_| "session send timed out".to_owned())
                    .and_then(|r| r.map_err(|e| e.to_string()));
                    if result.is_err() {
                        break;
                    }
                }
                SharingJob::Sent {
                    device: peer,
                    generation: Some(generation),
                    result,
                }
            });
        }
        let sender = &self.sharing_outgoing[device];
        if matches!(control, SessionControl::Image { .. }) {
            // At most one replacement and one active stream per peer. Rapid
            // target/copy changes cannot queue 64 full-size image originals.
            *sender.image.borrow_mut() = Some(control);
            match sender.control.try_send(SessionControl::ImageReady) {
                Ok(()) | Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => true,
                Err(_) => false,
            }
        } else {
            sender.control.try_send(control).is_ok()
        }
    }

    pub(super) fn update_clipboard_target(&mut self) {
        if self.sharing.primary() != Some(self.public_key_fingerprint.as_str())
            || self.sharing.phase() != lan_mouse_data::sharing::Phase::Running
        {
            return;
        }
        let target = match self.current_target {
            Some(handle) => self.client_manager.fingerprint(handle),
            None => Some(self.public_key_fingerprint.clone()),
        };
        let actions = self.clipboard.target(target);
        self.apply_clipboard_actions(actions);
    }

    pub(super) fn poll_clipboard(&mut self) {
        // Also pumps the Windows clipboard owner while reads run on a worker.
        self.native_clipboard.poll_owner();
        if self.clipboard_read_pending {
            return;
        }
        self.clipboard_read_pending = true;
        let raw = self.native_clipboard.raw();
        self.clipboard_jobs.spawn_blocking(move || {
            ClipboardJob::Read(crate::clipboard_native::NativeClipboard::read_raw(raw))
        });
    }
    pub(super) fn clipboard_failed(&mut self) {
        self.clipboard_read_error = true;
        self.publish_clipboard_status(self.clipboard_route_status);
    }
    pub(super) fn handle_clipboard_job(&mut self, job: ClipboardJob) {
        match job {
            ClipboardJob::Read(result) => {
                self.clipboard_read_pending = false;
                match result.and_then(|s| self.native_clipboard.finish_read(s)) {
                    Ok(Some(snapshot)) => {
                        self.clipboard_read_error = false;
                        self.publish_clipboard_status(self.clipboard_route_status);
                        let actions = self.clipboard.observe_contents(
                            snapshot.serial,
                            snapshot.text,
                            snapshot.image,
                        );
                        self.apply_clipboard_actions(actions);
                    }
                    Ok(None) => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => self.clipboard_failed(),
                }
            }
            ClipboardJob::Prepared {
                revision,
                request,
                image,
                serial,
                result,
            } => {
                if !self.clipboard.can_write_image(revision, &request, &image)
                    || !self.native_clipboard.is_current(serial).unwrap_or(false)
                {
                    return;
                }
                match result.and_then(|p| self.native_clipboard.write_image(serial, &p)) {
                    Ok(Some(snapshot)) => {
                        let actions = self.clipboard.written(revision, *request, snapshot.serial);
                        self.apply_clipboard_actions(actions);
                    }
                    Ok(None) => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => self.publish_clipboard_status(ClipboardState::Failed),
                }
            }
        }
    }

    fn publish_clipboard_status(&mut self, status: ClipboardState) {
        use lan_mouse_ipc::ClipboardStatus as Ipc;
        self.clipboard_route_status = status;
        let status = if self.clipboard_read_error {
            ClipboardState::Failed
        } else {
            status
        };
        let status = match status {
            ClipboardState::Paused => Ipc::Paused,
            ClipboardState::Waiting => Ipc::Waiting,
            ClipboardState::Syncing => Ipc::Syncing,
            ClipboardState::Ready => Ipc::Ready,
            ClipboardState::Unavailable => Ipc::Unavailable,
            ClipboardState::Unsupported => Ipc::Unsupported,
            ClipboardState::Failed => Ipc::Failed,
        };
        if status != self.clipboard_status {
            self.clipboard_status = status;
            self.notify_frontend(FrontendEvent::ClipboardStatus(status));
        }
    }

    pub(super) fn apply_clipboard_actions(&mut self, actions: Vec<Action>) {
        let mut queue: VecDeque<_> = actions.into();
        while let Some(action) = queue.pop_front() {
            match action {
                Action::CancelImages => self
                    .clipboard_wire_epoch
                    .set(self.clipboard_wire_epoch.get().wrapping_add(1)),
                Action::SendImage {
                    device,
                    request,
                    revision,
                    image,
                } => {
                    let control = SessionControl::Image {
                        request,
                        revision,
                        bytes: image.encode(),
                        offset: 0,
                        epoch: self.clipboard_wire_epoch.get(),
                    };
                    if !self.queue_session_control(&device, control) {
                        self.publish_clipboard_status(ClipboardState::Unavailable);
                    }
                }
                Action::WriteImage {
                    revision,
                    request,
                    image,
                    expected_serial,
                } => {
                    if self.clipboard_jobs.len() >= 3 {
                        continue;
                    }
                    let directory = self.clipboard_asset_dir.clone();
                    self.clipboard_jobs.spawn_blocking(move || {
                        let result = crate::clipboard_native::NativeClipboard::prepare_image(
                            image.clone(),
                            &directory,
                        );
                        ClipboardJob::Prepared {
                            revision,
                            request: Box::new(request),
                            image,
                            serial: expected_serial,
                            result,
                        }
                    });
                }
                Action::Send { device, message } => {
                    if !self.send_session_control(&device, DataMessage::Clipboard { message }) {
                        self.data_sessions.remove(&device);
                        self.sharing_outgoing.remove(&device);
                        self.sharing_generations.remove(&device);
                        self.clipboard.disconnected(&device);
                        let effects = self.sharing.disconnected(&device);
                        self.apply_sharing_effects(effects);
                        self.publish_clipboard_status(ClipboardState::Unavailable);
                        return;
                    }
                }
                Action::Status(status) => self.publish_clipboard_status(status),
                Action::Write {
                    revision,
                    request,
                    text,
                    expected_serial,
                } => {
                    // No await between the final native read, session check
                    // and native write. A genuine local copy cancels this action.
                    if !self
                        .native_clipboard
                        .is_current(expected_serial)
                        .unwrap_or(false)
                        || !self.clipboard.can_write(revision, &request, &text)
                    {
                        continue;
                    }
                    match self.native_clipboard.write(expected_serial, &text) {
                        Ok(Some(snapshot)) => {
                            queue.extend(self.clipboard.written(revision, request, snapshot.serial))
                        }
                        Ok(None) => {}
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                        Err(_) => self.publish_clipboard_status(ClipboardState::Failed),
                    }
                }
            }
        }
    }
}

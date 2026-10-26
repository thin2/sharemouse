use super::*;
use lan_mouse_data::sharing::Phase;
use lan_mouse_ipc::{SharingPhase, SharingState};

impl Service {
    pub(super) fn fail_sharing(&mut self, reason: String) {
        self.input_gate.close();
        let actions = self.clipboard.pause();
        self.apply_clipboard_actions(actions);
        self.set_current_target(None);
        // Keep failure visible and reject stale Start messages. Do not
        // recursively enqueue another failing native release operation.
        let _ = self
            .sharing
            .disconnected(&self.public_key_fingerprint.clone());
        self.publish_sharing_state();
        self.notify_frontend(FrontendEvent::Error(reason));
    }

    pub(super) fn retry_sharing_connections(&mut self) {
        for (device, &addr) in &self.sharing_addresses {
            if self.data_sessions.is_connected(device)
                || self.sharing_connecting.contains(device)
                || !self
                    .allowed_fingerprints
                    .read()
                    .expect("lock")
                    .contains(device)
            {
                continue;
            }
            self.sharing_connecting.insert(device.clone());
            let device = device.clone();
            let sessions = self.data_sessions.clone();
            self.sharing_jobs.spawn_local(async move {
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    sessions.connect(device.clone(), addr),
                )
                .await
                .map_err(|_| "connection timed out".to_owned())
                .and_then(|r| r.map_err(|e| e.to_string()));
                SharingJob::Connected { device, result }
            });
        }
    }
    pub(super) fn request_sharing(&mut self, primary: Option<String>) {
        match self.sharing.request(primary) {
            Ok(effects) => self.apply_sharing_effects(effects),
            Err(reason) => self.notify_frontend(FrontendEvent::Error(reason.into())),
        }
    }

    pub(super) fn sharing_connected(&mut self, device: &str) {
        let Some(generation) = self.data_sessions.generation(device) else {
            return;
        };
        if self.sharing_generations.get(device) == Some(&generation) {
            return;
        }
        self.sharing_outgoing.remove(device);
        self.sharing_generations.insert(device.into(), generation);
        let mut effects = self.sharing.disconnected(device);
        effects.push(SharingEffect::Send {
            device: device.into(),
            message: self.sharing.hello(),
        });
        self.apply_sharing_effects(effects);
        let actions = self.clipboard.connected(device.into(), generation);
        self.apply_clipboard_actions(actions);
    }

    pub(super) fn refresh_sharing_members(&mut self) {
        let mut members: std::collections::BTreeSet<_> = self
            .allowed_fingerprints
            .read()
            .expect("lock")
            .iter()
            .cloned()
            .collect();
        members.insert(self.public_key_fingerprint.clone());
        if &members == self.sharing.members() {
            return;
        }
        self.input_gate.close();
        let mut epoch = [0; 16];
        if rustls::crypto::ring::default_provider()
            .secure_random
            .fill(&mut epoch)
            .is_err()
        {
            self.notify_frontend(FrontendEvent::Error(
                "could not generate input session identity; restart required".into(),
            ));
            return;
        }
        self.sharing = Sharing::new(self.public_key_fingerprint.clone(), members, epoch);
        self.sharing_generations.clear();
        self.sharing_outgoing.clear();
        self.apply_sharing_effects(vec![SharingEffect::Fence { round: None }]);
        for device in self.data_sessions.connected_devices() {
            self.sharing_connected(&device);
        }
        let effects = self.sharing.initialize();
        self.apply_sharing_effects(effects);
    }

    pub(super) fn apply_sharing_effects(&mut self, effects: Vec<SharingEffect>) {
        let mut queue: VecDeque<_> = effects.into();
        while let Some(effect) = queue.pop_front() {
            match effect {
                SharingEffect::Send { device, message }
                    if device == self.public_key_fingerprint =>
                {
                    queue.extend(self.sharing.receive(&device, message));
                }
                SharingEffect::Send { device, message } => {
                    if !self.send_session_control(&device, DataMessage::Sharing { message }) {
                        self.sharing_outgoing.remove(&device);
                        self.data_sessions.remove(&device);
                        self.sharing_generations.remove(&device);
                        self.clipboard.disconnected(&device);
                        queue.extend(self.sharing.disconnected(&device));
                        self.notify_frontend(FrontendEvent::DataSessionDisconnected {
                            device,
                            reason: "input session control queue closed or full".into(),
                        });
                    }
                }
                SharingEffect::Fence { round } => {
                    self.input_gate.close();
                    let actions = self.clipboard.pause();
                    self.apply_clipboard_actions(actions);
                    self.set_current_target(None);
                    let primary = self.sharing.primary();
                    // Receivers also need edge capture to return to the primary.
                    let capture = self.capture.fence(primary.is_some());
                    let emulation = self
                        .emulation
                        .fence(primary.is_some() && primary != Some(&self.public_key_fingerprint));
                    self.sharing_jobs.spawn_local(async move {
                        let preparation = async move {
                            let (capture, emulation) = tokio::join!(capture, emulation);
                            capture.map_err(|_| "capture stopped without confirmation".to_owned())??;
                            emulation.map_err(|_| "emulation stopped without confirmation".to_owned())?
                        };
                        let result = tokio::time::timeout(
                            std::time::Duration::from_secs(30), preparation,
                        ).await.unwrap_or_else(|_| Err(
                            "input backend initialization timed out; check system permissions and retry".into()
                        ));
                        SharingJob::Released { round, result }
                    });
                }
                SharingEffect::Install { round, primary } => {
                    self.input_gate.install(primary, round.token())
                }
                SharingEffect::Start { round, primary } => {
                    // A stop session has no admitted primary.
                    if let (Some(primary), true) =
                        (primary, self.input_gate.token() == Some(round.token()))
                    {
                        self.input_gate.start(&self.public_key_fingerprint);
                        self.poll_clipboard();
                        let session = lan_mouse_data::clipboard::SessionId {
                            primary,
                            nonce: round.token().iter().map(|b| format!("{b:02x}")).collect(),
                        };
                        let actions = self
                            .clipboard
                            .start(session, self.sharing.members().iter().cloned().collect());
                        self.apply_clipboard_actions(actions);
                        self.update_clipboard_target();
                    }
                }
            }
        }
        self.publish_sharing_state();
    }

    pub(super) fn handle_sharing_job(&mut self, job: SharingJob) {
        match job {
            SharingJob::Connected { device, result } => {
                self.sharing_connecting.remove(&device);
                if let Err(reason) = result {
                    log::debug!("data reconnect: {reason}");
                }
            }
            SharingJob::Released {
                round: Some(round),
                result: Ok(()),
            } => {
                let effects = self.sharing.released(round);
                self.apply_sharing_effects(effects);
            }
            SharingJob::Released {
                round: Some(round),
                result: Err(reason),
            } if self.sharing.round() != Some(round) => {
                log::debug!("ignoring stale input preparation failure: {reason}");
            }
            SharingJob::Released {
                result: Err(reason),
                ..
            } => {
                if self.sharing.primary().is_some() {
                    self.stop_sharing();
                    self.notify_frontend(FrontendEvent::Error(format!(
                        "Input backend unavailable: {reason}"
                    )));
                } else {
                    self.fail_sharing(format!("input release was not confirmed: {reason}"));
                }
            }
            SharingJob::Sent {
                device,
                generation,
                result: Err(reason),
            } if self.data_sessions.generation(&device) == generation => {
                log::warn!("input session with {device}: {reason}");
                self.data_sessions.remove(&device);
                self.sharing_generations.remove(&device);
                self.sharing_outgoing.remove(&device);
                self.clipboard.disconnected(&device);
                let effects = self.sharing.disconnected(&device);
                self.apply_sharing_effects(effects);
                self.notify_frontend(FrontendEvent::DataSessionDisconnected { device, reason });
            }
            _ => {}
        }
    }

    fn publish_sharing_state(&mut self) {
        let primary = self.sharing.primary().map(str::to_owned);
        let phase = match self.sharing.phase() {
            Phase::Waiting => SharingPhase::Waiting,
            Phase::Stopping => SharingPhase::Stopping,
            Phase::Installing | Phase::Ready => SharingPhase::Starting,
            Phase::Stopped => SharingPhase::Stopped,
            Phase::Running => SharingPhase::Running,
        };
        let state = SharingState {
            phase,
            local_primary: primary.as_ref() == Some(&self.public_key_fingerprint),
            primary_name: primary.as_ref().and_then(|p| self.device_name(p)),
            primary,
        };
        if self.sharing_last_state != state {
            self.sharing_last_state = state.clone();
            self.notify_frontend(FrontendEvent::SharingState(state));
        }
    }
}

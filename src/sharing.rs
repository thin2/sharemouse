use std::{cell::RefCell, rc::Rc};

use lan_mouse_proto::SessionToken;

/// Shared on the service's LocalSet. Closing this gate takes effect before
/// queued capture, datagram or native emulation work can run.
#[derive(Clone, Default)]
pub(crate) struct InputGate(Rc<RefCell<Admission>>);

#[derive(Default)]
struct Admission {
    session: Option<(String, SessionToken)>,
    sending: bool,
}

impl InputGate {
    pub(crate) fn close(&self) {
        *self.0.borrow_mut() = Admission::default();
    }
    pub(crate) fn install(&self, primary: Option<String>, token: SessionToken) {
        *self.0.borrow_mut() = Admission {
            session: primary.map(|p| (p, token)),
            sending: false,
        };
    }
    pub(crate) fn start(&self, local: &str) {
        let mut state = self.0.borrow_mut();
        state.sending = state.session.as_ref().is_some_and(|(p, _)| p == local);
    }
    pub(crate) fn sending(&self) -> Option<SessionToken> {
        let state = self.0.borrow();
        state
            .sending
            .then(|| state.session.as_ref().map(|(_, t)| *t))
            .flatten()
    }
    pub(crate) fn accepts(&self, source: &str, token: SessionToken) -> bool {
        self.0
            .borrow()
            .session
            .as_ref()
            .is_some_and(|(p, t)| p == source && *t == token)
    }
    pub(crate) fn token(&self) -> Option<SessionToken> {
        self.0.borrow().session.as_ref().map(|(_, t)| *t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_selected_primary_and_current_token_are_admitted() {
        let gate = InputGate::default();
        gate.install(Some("a".into()), [1; 24]);
        assert!(gate.sending().is_none());
        assert!(gate.accepts("a", [1; 24]));
        assert!(!gate.accepts("b", [1; 24]));
        assert!(!gate.accepts("a", [0; 24]));
        gate.start("b");
        assert!(gate.sending().is_none());
        gate.start("a");
        assert_eq!(gate.sending(), Some([1; 24]));
        let other_task = gate.clone();
        gate.close();
        assert!(other_task.sending().is_none());
        assert!(!other_task.accepts("a", [1; 24]));
        gate.install(Some("b".into()), [2; 24]);
        gate.start("a");
        assert!(gate.sending().is_none());
        assert!(!gate.accepts("a", [1; 24]));
    }
}

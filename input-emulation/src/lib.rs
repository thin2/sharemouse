use async_trait::async_trait;
use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
};

use input_event::{Event, KeyboardEvent, PointerEvent};

pub use self::error::{EmulationCreationError, EmulationError, InputEmulationError};

#[cfg(windows)]
mod windows;

#[cfg(x11)]
mod x11;

#[cfg(wlroots)]
mod wlroots;

#[cfg(rdp)]
mod xdg_desktop_portal;

#[cfg(libei)]
mod libei;

#[cfg(target_os = "macos")]
mod macos;

/// fallback input emulation (logs events)
mod dummy;
mod error;

pub type EmulationHandle = u64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backend {
    #[cfg(wlroots)]
    Wlroots,
    #[cfg(libei)]
    Libei,
    #[cfg(rdp)]
    Xdp,
    #[cfg(x11)]
    X11,
    #[cfg(windows)]
    Windows,
    #[cfg(target_os = "macos")]
    MacOs,
    Dummy,
}

impl Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(wlroots)]
            Backend::Wlroots => write!(f, "wlroots"),
            #[cfg(libei)]
            Backend::Libei => write!(f, "libei"),
            #[cfg(rdp)]
            Backend::Xdp => write!(f, "xdg-desktop-portal"),
            #[cfg(x11)]
            Backend::X11 => write!(f, "X11"),
            #[cfg(windows)]
            Backend::Windows => write!(f, "windows"),
            #[cfg(target_os = "macos")]
            Backend::MacOs => write!(f, "macos"),
            Backend::Dummy => write!(f, "dummy"),
        }
    }
}

pub struct InputEmulation {
    emulation: Box<dyn Emulation>,
    handles: HashSet<EmulationHandle>,
    pressed_keys: HashMap<EmulationHandle, HashSet<u32>>,
    pressed_buttons: HashMap<EmulationHandle, HashSet<u32>>,
}

impl InputEmulation {
    async fn with_backend(backend: Backend) -> Result<InputEmulation, EmulationCreationError> {
        let emulation: Box<dyn Emulation> = match backend {
            #[cfg(wlroots)]
            Backend::Wlroots => Box::new(wlroots::WlrootsEmulation::new()?),
            #[cfg(libei)]
            Backend::Libei => Box::new(libei::LibeiEmulation::new().await?),
            #[cfg(x11)]
            Backend::X11 => Box::new(x11::X11Emulation::new()?),
            #[cfg(rdp)]
            Backend::Xdp => Box::new(xdg_desktop_portal::DesktopPortalEmulation::new().await?),
            #[cfg(windows)]
            Backend::Windows => Box::new(windows::WindowsEmulation::new()?),
            #[cfg(target_os = "macos")]
            Backend::MacOs => Box::new(macos::MacOSEmulation::new()?),
            Backend::Dummy => Box::new(dummy::DummyEmulation::new()),
        };
        Ok(Self {
            emulation,
            handles: HashSet::new(),
            pressed_keys: HashMap::new(),
            pressed_buttons: HashMap::new(),
        })
    }

    pub async fn new(backend: Option<Backend>) -> Result<InputEmulation, EmulationCreationError> {
        if let Some(backend) = backend {
            let b = Self::with_backend(backend).await;
            if b.is_ok() {
                log::info!("using emulation backend: {backend}");
            }
            return b;
        }

        let mut last_error = None;
        for backend in [
            #[cfg(wlroots)]
            Backend::Wlroots,
            #[cfg(libei)]
            Backend::Libei,
            #[cfg(rdp)]
            Backend::Xdp,
            #[cfg(x11)]
            Backend::X11,
            #[cfg(windows)]
            Backend::Windows,
            #[cfg(target_os = "macos")]
            Backend::MacOs,
        ] {
            match Self::with_backend(backend).await {
                Ok(b) => {
                    log::info!("using emulation backend: {backend}");
                    return Ok(b);
                }
                Err(e) if e.cancelled_by_user() => return Err(e),
                Err(e) => {
                    log::warn!("{e}");
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap_or(EmulationCreationError::NoAvailableBackend))
    }

    pub async fn consume(
        &mut self,
        event: Event,
        handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        match event {
            Event::Keyboard(KeyboardEvent::Key { key, state, .. }) => {
                let Some(keys) = self.pressed_keys.get_mut(&handle) else {
                    return Ok(());
                };
                if keys.contains(&key) == (state != 0) {
                    return Ok(());
                }
                self.emulation.consume(event, handle).await?;
                if state == 0 {
                    keys.remove(&key);
                } else {
                    keys.insert(key);
                }
                Ok(())
            }
            Event::Pointer(PointerEvent::Button { button, state, .. }) => {
                self.emulation.consume(event, handle).await?;
                if let Some(buttons) = self.pressed_buttons.get_mut(&handle) {
                    if state == 0 {
                        buttons.remove(&button);
                    } else {
                        buttons.insert(button);
                    }
                }
                Ok(())
            }
            _ => self.emulation.consume(event, handle).await,
        }
    }

    pub async fn create(&mut self, handle: EmulationHandle) -> bool {
        if self.handles.insert(handle) {
            self.pressed_keys.insert(handle, HashSet::new());
            self.pressed_buttons.insert(handle, HashSet::new());
            self.emulation.create(handle).await;
            true
        } else {
            false
        }
    }

    pub async fn destroy(&mut self, handle: EmulationHandle) {
        if let Err(error) = self.destroy_checked(handle).await {
            log::warn!("failed to release emulated input: {error}");
        }
    }

    /// Confirm release before destroying a handle. Failed releases retain
    /// their tracking state and must not count as a successful stop.
    pub async fn destroy_checked(&mut self, handle: EmulationHandle) -> Result<(), EmulationError> {
        if !self.handles.contains(&handle) {
            return Ok(());
        }
        self.release_keys(handle).await?;
        if self.handles.remove(&handle) {
            self.pressed_keys.remove(&handle);
            self.pressed_buttons.remove(&handle);
            self.emulation.destroy(handle).await
        }
        Ok(())
    }

    pub async fn terminate(&mut self) {
        if let Err(error) = self.terminate_checked().await {
            log::warn!("failed to release emulated input during termination: {error}");
        }
    }

    pub async fn terminate_checked(&mut self) -> Result<(), EmulationError> {
        let mut result = Ok(());
        for handle in self.handles.iter().cloned().collect::<Vec<_>>() {
            if let Err(error) = self.destroy_checked(handle).await {
                result = Err(error);
            }
        }
        self.emulation.terminate().await;
        result
    }

    pub async fn release_keys(&mut self, handle: EmulationHandle) -> Result<(), EmulationError> {
        if let Some(keys) = self.pressed_keys.get_mut(&handle) {
            for key in keys.iter().copied().collect::<Vec<_>>() {
                let event = Event::Keyboard(KeyboardEvent::Key {
                    time: 0,
                    key,
                    state: 0,
                });
                self.emulation.consume(event, handle).await?;
                self.pressed_keys
                    .get_mut(&handle)
                    .expect("registered handle")
                    .remove(&key);
                if let Ok(key) = input_event::scancode::Linux::try_from(key) {
                    log::warn!("releasing stuck key: {key:?}");
                }
            }
        }
        if let Some(buttons) = self.pressed_buttons.get_mut(&handle) {
            for button in buttons.iter().copied().collect::<Vec<_>>() {
                self.emulation
                    .consume(
                        Event::Pointer(PointerEvent::Button {
                            time: 0,
                            button,
                            state: 0,
                        }),
                        handle,
                    )
                    .await?;
                self.pressed_buttons
                    .get_mut(&handle)
                    .expect("registered handle")
                    .remove(&button);
            }
        }

        let event = Event::Keyboard(KeyboardEvent::Modifiers {
            depressed: 0,
            latched: 0,
            locked: 0,
            group: 0,
        });
        self.emulation.consume(event, handle).await?;
        Ok(())
    }

    pub fn has_pressed_keys(&self, handle: EmulationHandle) -> bool {
        self.pressed_keys
            .get(&handle)
            .is_some_and(|p| !p.is_empty())
    }
}

#[async_trait]
trait Emulation: Send {
    async fn consume(
        &mut self,
        event: Event,
        handle: EmulationHandle,
    ) -> Result<(), EmulationError>;
    async fn create(&mut self, handle: EmulationHandle);
    async fn destroy(&mut self, handle: EmulationHandle);
    async fn terminate(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Recording {
        events: Vec<Event>,
        fail: bool,
        destroyed: bool,
    }

    struct BackendRecorder(Arc<Mutex<Recording>>);

    #[async_trait]
    impl Emulation for BackendRecorder {
        async fn consume(
            &mut self,
            event: Event,
            _: EmulationHandle,
        ) -> Result<(), EmulationError> {
            let mut record = self.0.lock().unwrap();
            if record.fail {
                return Err(EmulationError::EndOfStream);
            }
            record.events.push(event);
            Ok(())
        }
        async fn create(&mut self, _: EmulationHandle) {}
        async fn destroy(&mut self, _: EmulationHandle) {
            self.0.lock().unwrap().destroyed = true;
        }
        async fn terminate(&mut self) {}
    }

    fn setup() -> (InputEmulation, Arc<Mutex<Recording>>) {
        let record = Arc::new(Mutex::new(Recording::default()));
        (
            InputEmulation {
                emulation: Box::new(BackendRecorder(record.clone())),
                handles: HashSet::new(),
                pressed_keys: HashMap::new(),
                pressed_buttons: HashMap::new(),
            },
            record,
        )
    }

    #[tokio::test]
    async fn confirmed_stop_releases_keyboard_mouse_buttons_and_modifiers() {
        let (mut input, record) = setup();
        input.create(1).await;
        input
            .consume(
                Event::Keyboard(KeyboardEvent::Key {
                    time: 0,
                    key: 30,
                    state: 1,
                }),
                1,
            )
            .await
            .unwrap();
        input
            .consume(
                Event::Pointer(PointerEvent::Button {
                    time: 0,
                    button: 0x110,
                    state: 1,
                }),
                1,
            )
            .await
            .unwrap();
        input.destroy_checked(1).await.unwrap();
        let record = record.lock().unwrap();
        assert!(record.destroyed);
        assert!(record.events.contains(&Event::Keyboard(KeyboardEvent::Key {
            time: 0,
            key: 30,
            state: 0
        })));
        assert!(
            record
                .events
                .contains(&Event::Pointer(PointerEvent::Button {
                    time: 0,
                    button: 0x110,
                    state: 0
                }))
        );
        assert!(
            record
                .events
                .contains(&Event::Keyboard(KeyboardEvent::Modifiers {
                    depressed: 0,
                    latched: 0,
                    locked: 0,
                    group: 0
                }))
        );
        assert!(input.handles.is_empty());
    }

    #[tokio::test]
    async fn failed_native_release_is_not_confirmed_and_can_be_retried() {
        let (mut input, record) = setup();
        input.create(1).await;
        input
            .consume(
                Event::Keyboard(KeyboardEvent::Key {
                    time: 0,
                    key: 30,
                    state: 1,
                }),
                1,
            )
            .await
            .unwrap();
        record.lock().unwrap().fail = true;
        assert!(input.destroy_checked(1).await.is_err());
        assert!(input.has_pressed_keys(1));
        assert!(!record.lock().unwrap().destroyed);
        record.lock().unwrap().fail = false;
        input.destroy_checked(1).await.unwrap();
        assert!(!input.has_pressed_keys(1));
        assert!(record.lock().unwrap().destroyed);
    }

    #[tokio::test]
    async fn failed_key_up_preserves_pressed_state_until_successful_retry() {
        let (mut input, record) = setup();
        input.create(1).await;
        input
            .consume(
                Event::Keyboard(KeyboardEvent::Key {
                    time: 0,
                    key: 30,
                    state: 1,
                }),
                1,
            )
            .await
            .unwrap();
        record.lock().unwrap().fail = true;
        assert!(
            input
                .consume(
                    Event::Keyboard(KeyboardEvent::Key {
                        time: 0,
                        key: 30,
                        state: 0
                    }),
                    1
                )
                .await
                .is_err()
        );
        assert!(input.has_pressed_keys(1));
        record.lock().unwrap().fail = false;
        input
            .consume(
                Event::Keyboard(KeyboardEvent::Key {
                    time: 0,
                    key: 30,
                    state: 0,
                }),
                1,
            )
            .await
            .unwrap();
        assert!(!input.has_pressed_keys(1));
    }
}

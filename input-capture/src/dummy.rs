use std::f64::consts::PI;
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use async_trait::async_trait;
use futures_core::Stream;
use input_event::{BTN_LEFT, Event, KeyboardEvent, PointerEvent, scancode};
use tokio::time::{self, Instant, Interval};

use super::{Capture, CaptureError, CaptureEvent, Position};

pub struct DummyInputCapture {
    start: Option<Instant>,
    interval: Interval,
    offset: (i32, i32),
    sample: u64,
}

impl DummyInputCapture {
    pub fn new() -> Self {
        Self {
            start: None,
            interval: time::interval(Duration::from_millis(1)),
            offset: (0, 0),
            sample: 0,
        }
    }
}

impl Default for DummyInputCapture {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Capture for DummyInputCapture {
    async fn create(&mut self, _pos: Position) -> Result<(), CaptureError> {
        Ok(())
    }

    async fn destroy(&mut self, _pos: Position) -> Result<(), CaptureError> {
        Ok(())
    }

    async fn release(&mut self) -> Result<(), CaptureError> {
        // Simulate another edge entry after the caller releases this grab.
        self.start = None;
        self.sample = 0;
        Ok(())
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }
}

const FREQUENCY_HZ: f64 = 1.0;
const RADIUS: f64 = 100.0;

impl Stream for DummyInputCapture {
    type Item = Result<(Position, CaptureEvent), CaptureError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let current = ready!(self.interval.poll_tick(cx));
        let event = match self.start {
            None => {
                self.start.replace(current);
                CaptureEvent::Begin
            }
            Some(start) => {
                self.sample += 1;
                // Exercise key/button releases as well as relative motion.
                let input = match self.sample % 100 {
                    20 | 21 => Some(Event::Keyboard(KeyboardEvent::Key {
                        time: 0,
                        key: scancode::Linux::KeyA as u32,
                        state: u8::from(self.sample % 100 == 20),
                    })),
                    22 | 23 => Some(Event::Pointer(PointerEvent::Button {
                        time: 0,
                        button: BTN_LEFT,
                        state: u32::from(self.sample % 100 == 22),
                    })),
                    _ => None,
                };
                if let Some(input) = input {
                    return Poll::Ready(Some(Ok((Position::Left, CaptureEvent::Input(input)))));
                }
                let elapsed = start.elapsed();
                let elapsed_sec_f64 = elapsed.as_secs_f64();
                let second_fraction = elapsed_sec_f64 - elapsed_sec_f64 as u64 as f64;
                let radians = second_fraction * 2. * PI * FREQUENCY_HZ;
                let offset = (radians.cos() * RADIUS * 2., (radians * 2.).sin() * RADIUS);
                let offset = (offset.0 as i32, offset.1 as i32);
                let relative_motion = (offset.0 - self.offset.0, offset.1 - self.offset.1);
                self.offset = offset;
                let (dx, dy) = (relative_motion.0 as f64, relative_motion.1 as f64);
                CaptureEvent::Input(input_event::Event::Pointer(PointerEvent::Motion {
                    time: 0,
                    dx,
                    dy,
                }))
            }
        };
        Poll::Ready(Some(Ok((Position::Left, event))))
    }
}

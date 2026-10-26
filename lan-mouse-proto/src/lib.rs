use input_event::{Event as InputEvent, KeyboardEvent, PointerEvent};
use num_enum::{IntoPrimitive, TryFromPrimitive, TryFromPrimitiveError};
use paste::paste;
use std::{
    fmt::{Debug, Display, Formatter},
    mem::size_of,
};
use thiserror::Error;

/// Largest event: pointer motion plus the session tag and 24-byte token.
pub const MAX_EVENT_SIZE: usize = size_of::<u8>() + size_of::<u32>() + 2 * size_of::<f64>() + 25;
pub type SessionToken = [u8; 24];

/// error type for protocol violations
#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("truncated or oversized event")]
    InvalidLength,
    #[error("invalid session event envelope")]
    InvalidEnvelope,
    /// event type does not exist
    #[error("invalid event id: `{0}`")]
    InvalidEventId(#[from] TryFromPrimitiveError<EventType>),
    /// position type does not exist
    #[error("invalid event id: `{0}`")]
    InvalidPosition(#[from] TryFromPrimitiveError<Position>),
}

/// Position of a client
#[derive(Clone, Copy, Debug, TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum Position {
    Left,
    Right,
    Top,
    Bottom,
}

impl Display for Position {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let pos = match self {
            Position::Left => "left",
            Position::Right => "right",
            Position::Top => "top",
            Position::Bottom => "bottom",
        };
        write!(f, "{pos}")
    }
}

/// main lan-mouse protocol event type
#[derive(Clone, Copy, Debug)]
pub enum ProtoEvent {
    SessionEnter(Position, SessionToken),
    SessionInput(InputEvent, SessionToken),
    SessionLeave(SessionToken),
    SessionAck(SessionToken),
    /// notify a client that the cursor entered its region at the given position
    /// [`ProtoEvent::Ack`] with the same serial is used for synchronization between devices
    Enter(Position),
    /// notify a client that the cursor left its region
    /// [`ProtoEvent::Ack`] with the same serial is used for synchronization between devices
    Leave(u32),
    /// acknowledge of an [`ProtoEvent::Enter`] or [`ProtoEvent::Leave`] event
    Ack(u32),
    /// Input event
    Input(InputEvent),
    /// Ping event for tracking unresponsive clients.
    /// A client has to respond with [`ProtoEvent::Pong`].
    Ping,
    /// Response to [`ProtoEvent::Ping`], true if emulation is enabled / available
    Pong(bool),
    /// Build identification for the sending peer. Sent by the
    /// connect side once after the connection authenticates, and
    /// echoed back by the listen side in reply, so each end can
    /// display the peer's build hash and warn (soft) on mismatch.
    /// `commit` is the 8-byte ASCII short commit hash from
    /// `shadow_rs`'s `SHORT_COMMIT`. Old peers that don't
    /// recognize the event type silently skip it per the
    /// forward-compat handling in the receive loop.
    Hello {
        commit: [u8; 8],
    },
}

impl Display for ProtoEvent {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtoEvent::SessionEnter(pos, _) => write!(f, "session enter({pos})"),
            ProtoEvent::SessionInput(event, _) => write!(f, "session {event}"),
            ProtoEvent::SessionLeave(_) => write!(f, "session leave"),
            ProtoEvent::SessionAck(_) => write!(f, "session ack"),
            ProtoEvent::Enter(s) => write!(f, "Enter({s})"),
            ProtoEvent::Leave(s) => write!(f, "Leave({s})"),
            ProtoEvent::Ack(s) => write!(f, "Ack({s})"),
            ProtoEvent::Input(e) => write!(f, "{e}"),
            ProtoEvent::Ping => write!(f, "ping"),
            ProtoEvent::Pong(alive) => {
                write!(
                    f,
                    "pong: {}",
                    if *alive { "alive" } else { "not available" }
                )
            }
            ProtoEvent::Hello { commit } => {
                let s = std::str::from_utf8(commit).unwrap_or("????????");
                write!(f, "Hello({s})")
            }
        }
    }
}

#[derive(TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum EventType {
    PointerMotion,
    PointerButton,
    PointerAxis,
    PointerAxisValue120,
    KeyboardKey,
    KeyboardModifiers,
    Ping,
    Pong,
    Enter,
    Leave,
    Ack,
    Hello,
    Session,
}

impl ProtoEvent {
    /// Decode exactly one datagram. Padding from a previous receive must
    /// never supply missing bytes, particularly part of a session token.
    pub fn decode(datagram: &[u8]) -> Result<Self, ProtocolError> {
        if datagram.is_empty() || datagram.len() > MAX_EVENT_SIZE {
            return Err(ProtocolError::InvalidLength);
        }
        let mut buf = [0; MAX_EVENT_SIZE];
        buf[..datagram.len()].copy_from_slice(datagram);
        let event = Self::try_from(buf)?;
        let (_, expected): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        if datagram.len() != expected {
            return Err(ProtocolError::InvalidLength);
        }
        Ok(event)
    }

    fn event_type(&self) -> EventType {
        match self {
            ProtoEvent::SessionEnter(..)
            | ProtoEvent::SessionInput(..)
            | ProtoEvent::SessionLeave(..)
            | ProtoEvent::SessionAck(..) => EventType::Session,
            ProtoEvent::Input(e) => match e {
                InputEvent::Pointer(p) => match p {
                    PointerEvent::Motion { .. } => EventType::PointerMotion,
                    PointerEvent::Button { .. } => EventType::PointerButton,
                    PointerEvent::Axis { .. } => EventType::PointerAxis,
                    PointerEvent::AxisDiscrete120 { .. } => EventType::PointerAxisValue120,
                },
                InputEvent::Keyboard(k) => match k {
                    KeyboardEvent::Key { .. } => EventType::KeyboardKey,
                    KeyboardEvent::Modifiers { .. } => EventType::KeyboardModifiers,
                },
            },
            ProtoEvent::Ping => EventType::Ping,
            ProtoEvent::Pong(_) => EventType::Pong,
            ProtoEvent::Enter(_) => EventType::Enter,
            ProtoEvent::Leave(_) => EventType::Leave,
            ProtoEvent::Ack(_) => EventType::Ack,
            ProtoEvent::Hello { .. } => EventType::Hello,
        }
    }
}

impl TryFrom<[u8; MAX_EVENT_SIZE]> for ProtoEvent {
    type Error = ProtocolError;

    fn try_from(buf: [u8; MAX_EVENT_SIZE]) -> Result<Self, Self::Error> {
        let mut buf = &buf[..];
        let event_type = decode_u8(&mut buf)?;
        match EventType::try_from(event_type)? {
            EventType::Session => {
                let mut token = [0; 24];
                for b in &mut token {
                    *b = decode_u8(&mut buf)?;
                }
                let mut nested = [0; MAX_EVENT_SIZE];
                nested[..buf.len()].copy_from_slice(buf);
                // Only a single envelope is allowed, preventing recursive decoding.
                match nested[0] {
                    v if v == EventType::Enter as u8
                        || v <= EventType::KeyboardModifiers as u8
                        || v == EventType::Leave as u8
                        || v == EventType::Ack as u8 => {}
                    _ => return Err(ProtocolError::InvalidEnvelope),
                }
                match Self::try_from(nested)? {
                    Self::Enter(pos) => Ok(Self::SessionEnter(pos, token)),
                    Self::Input(event) => Ok(Self::SessionInput(event, token)),
                    Self::Leave(_) => Ok(Self::SessionLeave(token)),
                    Self::Ack(_) => Ok(Self::SessionAck(token)),
                    _ => unreachable!(),
                }
            }
            EventType::PointerMotion => {
                Ok(Self::Input(InputEvent::Pointer(PointerEvent::Motion {
                    time: decode_u32(&mut buf)?,
                    dx: decode_f64(&mut buf)?,
                    dy: decode_f64(&mut buf)?,
                })))
            }
            EventType::PointerButton => {
                Ok(Self::Input(InputEvent::Pointer(PointerEvent::Button {
                    time: decode_u32(&mut buf)?,
                    button: decode_u32(&mut buf)?,
                    state: decode_u32(&mut buf)?,
                })))
            }
            EventType::PointerAxis => Ok(Self::Input(InputEvent::Pointer(PointerEvent::Axis {
                time: decode_u32(&mut buf)?,
                axis: decode_u8(&mut buf)?,
                value: decode_f64(&mut buf)?,
            }))),
            EventType::PointerAxisValue120 => Ok(Self::Input(InputEvent::Pointer(
                PointerEvent::AxisDiscrete120 {
                    axis: decode_u8(&mut buf)?,
                    value: decode_i32(&mut buf)?,
                },
            ))),
            EventType::KeyboardKey => Ok(Self::Input(InputEvent::Keyboard(KeyboardEvent::Key {
                time: decode_u32(&mut buf)?,
                key: decode_u32(&mut buf)?,
                state: decode_u8(&mut buf)?,
            }))),
            EventType::KeyboardModifiers => Ok(Self::Input(InputEvent::Keyboard(
                KeyboardEvent::Modifiers {
                    depressed: decode_u32(&mut buf)?,
                    latched: decode_u32(&mut buf)?,
                    locked: decode_u32(&mut buf)?,
                    group: decode_u32(&mut buf)?,
                },
            ))),
            EventType::Ping => Ok(Self::Ping),
            EventType::Pong => Ok(Self::Pong(decode_u8(&mut buf)? != 0)),
            EventType::Enter => Ok(Self::Enter(decode_u8(&mut buf)?.try_into()?)),
            EventType::Leave => Ok(Self::Leave(decode_u32(&mut buf)?)),
            EventType::Ack => Ok(Self::Ack(decode_u32(&mut buf)?)),
            EventType::Hello => {
                let mut commit = [0u8; 8];
                for b in commit.iter_mut() {
                    *b = decode_u8(&mut buf)?;
                }
                Ok(Self::Hello { commit })
            }
        }
    }
}

impl From<ProtoEvent> for ([u8; MAX_EVENT_SIZE], usize) {
    fn from(event: ProtoEvent) -> Self {
        let mut buf = [0u8; MAX_EVENT_SIZE];
        let mut len = 0usize;
        {
            let mut buf = &mut buf[..];
            let buf = &mut buf;
            let len = &mut len;
            encode_u8(buf, len, event.event_type() as u8);
            match event {
                ProtoEvent::SessionEnter(_, token)
                | ProtoEvent::SessionInput(_, token)
                | ProtoEvent::SessionLeave(token)
                | ProtoEvent::SessionAck(token) => {
                    for byte in token {
                        encode_u8(buf, len, byte);
                    }
                    let nested = match event {
                        ProtoEvent::SessionEnter(pos, _) => ProtoEvent::Enter(pos),
                        ProtoEvent::SessionInput(event, _) => ProtoEvent::Input(event),
                        ProtoEvent::SessionLeave(_) => ProtoEvent::Leave(0),
                        ProtoEvent::SessionAck(_) => ProtoEvent::Ack(0),
                        _ => unreachable!(),
                    };
                    let (data, count): ([u8; MAX_EVENT_SIZE], usize) = nested.into();
                    for &byte in &data[..count] {
                        encode_u8(buf, len, byte);
                    }
                }
                ProtoEvent::Input(event) => match event {
                    InputEvent::Pointer(p) => match p {
                        PointerEvent::Motion { time, dx, dy } => {
                            encode_u32(buf, len, time);
                            encode_f64(buf, len, dx);
                            encode_f64(buf, len, dy);
                        }
                        PointerEvent::Button {
                            time,
                            button,
                            state,
                        } => {
                            encode_u32(buf, len, time);
                            encode_u32(buf, len, button);
                            encode_u32(buf, len, state);
                        }
                        PointerEvent::Axis { time, axis, value } => {
                            encode_u32(buf, len, time);
                            encode_u8(buf, len, axis);
                            encode_f64(buf, len, value);
                        }
                        PointerEvent::AxisDiscrete120 { axis, value } => {
                            encode_u8(buf, len, axis);
                            encode_i32(buf, len, value);
                        }
                    },
                    InputEvent::Keyboard(k) => match k {
                        KeyboardEvent::Key { time, key, state } => {
                            encode_u32(buf, len, time);
                            encode_u32(buf, len, key);
                            encode_u8(buf, len, state);
                        }
                        KeyboardEvent::Modifiers {
                            depressed,
                            latched,
                            locked,
                            group,
                        } => {
                            encode_u32(buf, len, depressed);
                            encode_u32(buf, len, latched);
                            encode_u32(buf, len, locked);
                            encode_u32(buf, len, group);
                        }
                    },
                },
                ProtoEvent::Ping => {}
                ProtoEvent::Pong(alive) => encode_u8(buf, len, alive as u8),
                ProtoEvent::Enter(pos) => encode_u8(buf, len, pos as u8),
                ProtoEvent::Leave(serial) => encode_u32(buf, len, serial),
                ProtoEvent::Ack(serial) => encode_u32(buf, len, serial),
                ProtoEvent::Hello { commit } => {
                    for b in commit.iter() {
                        encode_u8(buf, len, *b);
                    }
                }
            }
        }
        (buf, len)
    }
}

macro_rules! decode_impl {
    ($t:ty) => {
        paste! {
            fn [<decode_ $t>](data: &mut &[u8]) -> Result<$t, ProtocolError> {
                let (int_bytes, rest) = data.split_at(size_of::<$t>());
                *data = rest;
                Ok($t::from_be_bytes(int_bytes.try_into().unwrap()))
            }
        }
    };
}

decode_impl!(u8);
decode_impl!(u32);
decode_impl!(i32);
decode_impl!(f64);

macro_rules! encode_impl {
    ($t:ty) => {
        paste! {
            fn [<encode_ $t>](buf: &mut &mut [u8], amt: &mut usize, n: $t) {
                let src = n.to_be_bytes();
                let data = std::mem::take(buf);
                let (int_bytes, rest) = data.split_at_mut(size_of::<$t>());
                int_bytes.copy_from_slice(&src);
                *amt += size_of::<$t>();
                *buf = rest
            }
        }
    };
}

encode_impl!(u8);
encode_impl!(u32);
encode_impl!(i32);
encode_impl!(f64);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_events_preserve_tokens_and_input() {
        let motion = InputEvent::Pointer(PointerEvent::Motion {
            time: 123,
            dx: 1.25,
            dy: -4.5,
        });
        for event in [
            ProtoEvent::SessionEnter(Position::Right, [7; 24]),
            ProtoEvent::SessionInput(motion, [7; 24]),
            ProtoEvent::SessionLeave([7; 24]),
            ProtoEvent::SessionAck([7; 24]),
        ] {
            let (data, count): ([u8; MAX_EVENT_SIZE], usize) = event.into();
            let decoded = ProtoEvent::decode(&data[..count]).unwrap();
            let (encoded, length): ([u8; MAX_EVENT_SIZE], usize) = decoded.into();
            assert_eq!((encoded, length), (data, count));
            assert!(ProtoEvent::decode(&data[..count - 1]).is_err());
        }
    }

    #[test]
    fn nested_envelopes_and_trailing_bytes_are_rejected() {
        let mut nested = [0; MAX_EVENT_SIZE];
        nested[0] = EventType::Session as u8;
        nested[25] = EventType::Session as u8;
        assert!(matches!(
            ProtoEvent::decode(&nested),
            Err(ProtocolError::InvalidEnvelope)
        ));
        assert!(ProtoEvent::decode(&[EventType::Ping as u8, 0]).is_err());
        assert!(ProtoEvent::decode(&[]).is_err());
        assert!(matches!(
            ProtoEvent::decode(&[EventType::Ping as u8]),
            Ok(ProtoEvent::Ping)
        ));
    }
}

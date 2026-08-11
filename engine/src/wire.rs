//! Byte format for the event log.
//!
//! Hand-rolled. `serde` + a binary codec would be two dependencies and an
//! open question about format stability, in exchange for sixty lines of
//! obvious code. The format is part of the protocol: once two peers have
//! spoken it, changing a field means versioning it.
//!
//! Layout, little-endian:
//!
//! ```text
//! Start   0x00  seed(8)  w(1)  h(1)  mines(2)   = 13 bytes
//! Reveal  0x01  player(1)  x(1)  y(1)           =  4 bytes
//! Flag    0x02  player(1)  x(1)  y(1)           =  4 bytes
//! ```
//!
//! Every decode path is a trust boundary — the bytes arrive from a peer, and
//! a peer can be buggy, outdated, or hostile. Nothing here panics or indexes
//! unchecked; malformed input returns `None` and the caller drops the message.

use crate::Event;

const TAG_START: u8 = 0;
const TAG_REVEAL: u8 = 1;
const TAG_FLAG: u8 = 2;

const START_LEN: usize = 13;
const MOVE_LEN: usize = 4;

impl Event {
    pub fn encode(&self, out: &mut Vec<u8>) {
        match *self {
            Event::Start { seed, w, h, mines } => {
                out.push(TAG_START);
                out.extend_from_slice(&seed.to_le_bytes());
                out.push(w);
                out.push(h);
                out.extend_from_slice(&mines.to_le_bytes());
            }
            Event::Reveal { player, x, y } => out.extend_from_slice(&[TAG_REVEAL, player, x, y]),
            Event::Flag { player, x, y } => out.extend_from_slice(&[TAG_FLAG, player, x, y]),
        }
    }

    /// Decodes one event from the front of `bytes`, returning it and how many
    /// bytes it consumed. `None` means the input is truncated or malformed.
    pub fn decode(bytes: &[u8]) -> Option<(Event, usize)> {
        match *bytes.first()? {
            TAG_START if bytes.len() >= START_LEN => {
                let seed = u64::from_le_bytes(bytes[1..9].try_into().ok()?);
                let mines = u16::from_le_bytes(bytes[11..13].try_into().ok()?);
                let ev = Event::Start {
                    seed,
                    w: bytes[9],
                    h: bytes[10],
                    mines,
                };
                Some((ev, START_LEN))
            }
            tag @ (TAG_REVEAL | TAG_FLAG) if bytes.len() >= MOVE_LEN => {
                let (player, x, y) = (bytes[1], bytes[2], bytes[3]);
                let ev = if tag == TAG_REVEAL {
                    Event::Reveal { player, x, y }
                } else {
                    Event::Flag { player, x, y }
                };
                Some((ev, MOVE_LEN))
            }
            // Truncated, or a tag we don't know: a newer peer, or corruption.
            _ => None,
        }
    }
}

pub fn encode_log(events: &[Event]) -> Vec<u8> {
    let mut out = Vec::with_capacity(events.len() * MOVE_LEN);
    for ev in events {
        ev.encode(&mut out);
    }
    out
}

/// Decodes a whole log. Leftover or truncated bytes are a failure, not a
/// partial success — accepting half a log would desync the peers silently,
/// which is exactly the failure mode this format exists to prevent.
pub fn decode_log(bytes: &[u8]) -> Option<Vec<Event>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let (ev, n) = Event::decode(&bytes[i..])?;
        out.push(ev);
        i += n;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// If this test fails, you changed the wire format. That is allowed, but
    /// it means old peers can no longer talk to new ones — do it deliberately.
    #[test]
    fn start_layout_is_frozen() {
        let mut v = Vec::new();
        Event::Start {
            seed: 0x0102_0304_0506_0708,
            w: 9,
            h: 16,
            mines: 40,
        }
        .encode(&mut v);
        assert_eq!(v, [0, 8, 7, 6, 5, 4, 3, 2, 1, 9, 16, 40, 0]);
    }

    #[test]
    fn move_layout_is_frozen() {
        let mut v = Vec::new();
        Event::Reveal {
            player: 1,
            x: 2,
            y: 3,
        }
        .encode(&mut v);
        Event::Flag {
            player: 4,
            x: 5,
            y: 6,
        }
        .encode(&mut v);
        assert_eq!(v, [1, 1, 2, 3, 2, 4, 5, 6]);
    }

    /// Every partial frame must be rejected. Note this tests one *event*, not
    /// a log: truncating a log on an event boundary yields a valid shorter
    /// log, which is correct behaviour and not what we're checking here.
    #[test]
    fn a_partial_frame_is_always_rejected() {
        for ev in [
            Event::Start {
                seed: 7,
                w: 9,
                h: 9,
                mines: 10,
            },
            Event::Reveal {
                player: 0,
                x: 1,
                y: 2,
            },
        ] {
            let mut v = Vec::new();
            ev.encode(&mut v);
            for cut in 0..v.len() {
                assert!(
                    Event::decode(&v[..cut]).is_none(),
                    "accepted {cut} of {} bytes for {ev:?}",
                    v.len()
                );
            }
            assert_eq!(Event::decode(&v), Some((ev, v.len())));
        }
    }

    #[test]
    fn a_log_ending_mid_frame_is_rejected() {
        let full = encode_log(&[
            Event::Start {
                seed: 7,
                w: 9,
                h: 9,
                mines: 10,
            },
            Event::Reveal {
                player: 0,
                x: 1,
                y: 2,
            },
        ]);
        assert_eq!(full.len(), START_LEN + MOVE_LEN);
        // Cutting inside the second event: the first still decodes, the log
        // as a whole must not.
        for cut in START_LEN + 1..full.len() {
            assert!(
                decode_log(&full[..cut]).is_none(),
                "accepted a log truncated to {cut} bytes"
            );
        }
        // ...but cutting exactly on the boundary is a valid one-event log.
        assert_eq!(decode_log(&full[..START_LEN]).map(|v| v.len()), Some(1));
        assert_eq!(decode_log(&full).map(|v| v.len()), Some(2));
    }

    #[test]
    fn unknown_tag_is_rejected() {
        assert!(Event::decode(&[9, 0, 0, 0]).is_none());
    }

    #[test]
    fn trailing_garbage_is_rejected() {
        let mut v = encode_log(&[Event::Reveal {
            player: 0,
            x: 1,
            y: 2,
        }]);
        v.push(TAG_START); // a valid tag, but nothing behind it
        assert!(decode_log(&v).is_none());
    }

    #[test]
    fn empty_log_round_trips() {
        assert_eq!(decode_log(&encode_log(&[])), Some(vec![]));
    }

    fn any_event() -> impl Strategy<Value = Event> {
        prop_oneof![
            (any::<u64>(), any::<u8>(), any::<u8>(), any::<u16>())
                .prop_map(|(seed, w, h, mines)| Event::Start { seed, w, h, mines }),
            (any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(player, x, y)| Event::Reveal {
                player,
                x,
                y
            }),
            (any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(player, x, y)| Event::Flag {
                player,
                x,
                y
            }),
        ]
    }

    proptest! {
        #[test]
        fn every_event_round_trips(ev in any_event()) {
            let mut v = Vec::new();
            ev.encode(&mut v);
            let (back, n) = Event::decode(&v).unwrap();
            prop_assert_eq!(ev, back);
            prop_assert_eq!(n, v.len(), "decode disagreed with encode on length");
        }

        #[test]
        fn logs_round_trip(evs in prop::collection::vec(any_event(), 0..64)) {
            prop_assert_eq!(decode_log(&encode_log(&evs)), Some(evs));
        }

        /// The trust-boundary test: arbitrary bytes must never panic, only
        /// fail. Anything reachable from a DataChannel needs one of these.
        #[test]
        fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..256)) {
            let _ = decode_log(&bytes);
        }
    }
}

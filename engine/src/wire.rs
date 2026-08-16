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
//! Start   0x00  seed(8)  w(1)  h(1)  mines(2)  mode(1)  = 14 bytes
//! Reveal  0x01  player(1)  x(1)  y(1)           =  4 bytes
//! Flag    0x02  player(1)  x(1)  y(1)           =  4 bytes
//! ```
//!
//! The stamped-log framing around these — the `seq(4) at_ms(8)` prefix, the
//! message envelope, the all-or-nothing decode — lives in the `eventlog`
//! crate; this module only says what a Minesweeper move looks like in bytes,
//! and re-exports the framing specialised to it. The stamp prefix is version
//! 3 of this format; an older peer reading it sees a seq where it expects a
//! tag and rejects the message, which is the right failure.
//!
//! Every decode path is a trust boundary — the bytes arrive from a peer, and
//! a peer can be buggy, outdated, or hostile. Nothing here panics or indexes
//! unchecked; malformed input returns `None` and the caller drops the message.

use crate::{Event, Mode};

/// The message envelope, carrying Minesweeper moves.
pub type Msg = eventlog::Msg<Event>;
pub use eventlog::{decode_log, decode_msg, encode_log, encode_msg};

/// How events plug into the `eventlog` framing. `valid` is `is_playable`:
/// an event that decodes cleanly but describes an impossible game is refused
/// at the trust boundary rather than folded over later.
impl eventlog::Payload for Event {
    fn encode(&self, out: &mut Vec<u8>) {
        Event::encode(self, out);
    }
    fn decode(bytes: &[u8]) -> Option<(Event, usize)> {
        Event::decode(bytes)
    }
    fn valid(&self) -> bool {
        self.is_playable()
    }
}

const TAG_START: u8 = 0;
const TAG_REVEAL: u8 = 1;
const TAG_FLAG: u8 = 2;

const START_LEN: usize = 14;
const MOVE_LEN: usize = 4;

impl Event {
    pub fn encode(&self, out: &mut Vec<u8>) {
        match *self {
            Event::Start {
                seed,
                w,
                h,
                mines,
                mode,
            } => {
                out.push(TAG_START);
                out.extend_from_slice(&seed.to_le_bytes());
                out.push(w);
                out.push(h);
                out.extend_from_slice(&mines.to_le_bytes());
                out.push(mode as u8);
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
                    mode: Mode::from_u8(bytes[13])?,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Stamped;
    use eventlog::{STAMP_LEN, STATE_LEN};
    use proptest::prelude::*;

    fn stamp(seq: u32, ev: Event) -> Stamped {
        Stamped { seq, ev, at_ms: 0 }
    }

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
            mode: Mode::Coop,
        }
        .encode(&mut v);
        assert_eq!(v, [0, 8, 7, 6, 5, 4, 3, 2, 1, 9, 16, 40, 0, 0]);
    }

    /// The mode is the last byte of a Start, and a mode we do not know has to
    /// be refused rather than guessed at — the two peers would be playing
    /// different games with the same log.
    #[test]
    fn mode_round_trips_and_rejects_the_unknown() {
        for mode in [Mode::Coop, Mode::FlagRace, Mode::Race] {
            let mut v = Vec::new();
            Event::Start {
                seed: 1,
                w: 9,
                h: 9,
                mines: 10,
                mode,
            }
            .encode(&mut v);
            assert_eq!(*v.last().unwrap(), mode as u8);
            let (back, n) = Event::decode(&v).unwrap();
            assert_eq!(n, START_LEN);
            assert!(matches!(back, Event::Start { mode: m, .. } if m == mode));
        }

        let mut v = Vec::new();
        Event::Start {
            seed: 1,
            w: 9,
            h: 9,
            mines: 10,
            mode: Mode::Coop,
        }
        .encode(&mut v);
        *v.last_mut().unwrap() = 7; // a mode from a newer peer
        assert!(Event::decode(&v).is_none());
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
                mode: Mode::Coop,
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
        let start = STAMP_LEN + START_LEN;
        let full = encode_log(&[
            stamp(
                0,
                Event::Start {
                    seed: 7,
                    w: 9,
                    h: 9,
                    mines: 10,
                    mode: Mode::Coop,
                },
            ),
            stamp(
                1,
                Event::Reveal {
                    player: 0,
                    x: 1,
                    y: 2,
                },
            ),
        ]);
        assert_eq!(full.len(), start + STAMP_LEN + MOVE_LEN);
        // Cutting inside the second event: the first still decodes, the log
        // as a whole must not.
        for cut in start + 1..full.len() {
            assert!(
                decode_log::<Event>(&full[..cut]).is_none(),
                "accepted a log truncated to {cut} bytes"
            );
        }
        // ...but cutting exactly on the boundary is a valid one-event log.
        assert_eq!(
            decode_log::<Event>(&full[..start]).map(|v| v.len()),
            Some(1)
        );
        assert_eq!(decode_log::<Event>(&full).map(|v| v.len()), Some(2));
    }

    /// The stamp is what the peers sort by and what the clock is read from,
    /// so its bytes are frozen too: seq, then at_ms, then the event.
    #[test]
    fn stamp_layout_is_frozen() {
        let mut v = Vec::new();
        Stamped {
            seq: 0x0A0B_0C0D,
            at_ms: 0x0102_0304_0506_0708,
            ev: Event::Reveal {
                player: 1,
                x: 2,
                y: 3,
            },
        }
        .encode(&mut v);
        assert_eq!(
            v,
            [0x0D, 0x0C, 0x0B, 0x0A, 8, 7, 6, 5, 4, 3, 2, 1, 1, 1, 2, 3]
        );
    }

    #[test]
    fn a_partial_stamp_is_rejected() {
        let mut v = Vec::new();
        stamp(
            9,
            Event::Flag {
                player: 0,
                x: 1,
                y: 2,
            },
        )
        .encode(&mut v);
        for cut in 0..v.len() {
            assert!(Stamped::decode(&v[..cut]).is_none(), "accepted {cut} bytes");
        }
    }

    /// Sorting is the whole point: the order must not depend on which peer is
    /// doing the sorting, so equal `seq` still gives one deterministic answer.
    #[test]
    fn equal_seq_still_totally_orders() {
        let a = stamp(
            5,
            Event::Reveal {
                player: 0,
                x: 1,
                y: 1,
            },
        );
        let b = stamp(
            5,
            Event::Reveal {
                player: 1,
                x: 1,
                y: 1,
            },
        );
        assert!(a < b, "player breaks the tie");
        assert!(
            stamp(
                4,
                Event::Flag {
                    player: 9,
                    x: 0,
                    y: 0
                }
            ) < a,
            "seq outranks everything in the event"
        );
    }

    #[test]
    fn unknown_tag_is_rejected() {
        assert!(Event::decode(&[9, 0, 0, 0]).is_none());
    }

    #[test]
    fn trailing_garbage_is_rejected() {
        let mut v = encode_log(&[stamp(
            1,
            Event::Reveal {
                player: 0,
                x: 1,
                y: 2,
            },
        )]);
        v.push(TAG_START); // a valid tag, but nothing behind it
        assert!(decode_log::<Event>(&v).is_none());
    }

    #[test]
    fn state_message_round_trips() {
        let m = Msg::State {
            count: 7,
            hash: 0x0102_0304_0506_0708,
        };
        let bytes = encode_msg(&m);
        assert_eq!(bytes.len(), STATE_LEN);
        assert_eq!(decode_msg(&bytes), Some(m));
    }

    #[test]
    fn state_message_rejects_wrong_length() {
        let full = encode_msg(&Msg::State { count: 1, hash: 2 });
        for cut in 1..full.len() {
            assert!(
                decode_msg::<Event>(&full[..cut]).is_none(),
                "accepted {cut} bytes"
            );
        }
        let mut long = full.clone();
        long.push(0);
        assert!(
            decode_msg::<Event>(&long).is_none(),
            "accepted a trailing byte"
        );
    }

    #[test]
    fn unknown_message_kind_is_rejected() {
        assert!(decode_msg::<Event>(&[0x7F, 0, 0, 0]).is_none());
        assert!(decode_msg::<Event>(&[]).is_none());
    }

    #[test]
    fn empty_log_round_trips() {
        assert_eq!(decode_log::<Event>(&encode_log::<Event>(&[])), Some(vec![]));
    }

    /// Events a peer is allowed to play. `any_event` deliberately includes
    /// illegal ones — `Event::decode` round-trips whatever the bytes say, and
    /// it is `decode_msg` that refuses to hand them on.
    fn any_playable_event() -> impl Strategy<Value = Event> {
        prop_oneof![
            (
                any::<u64>(),
                // At least 3x3, so there is room for the safe zone and a mine.
                (3..=Event::MAX_W, 3..=Event::MAX_H).prop_flat_map(|(w, h)| {
                    let room = w as u16 * h as u16 - Event::SAFE_ZONE as u16;
                    (Just(w), Just(h), 0..=room)
                }),
                prop_oneof![Just(Mode::Coop), Just(Mode::FlagRace), Just(Mode::Race)],
            )
                .prop_map(|(seed, (w, h, mines), mode)| Event::Start {
                    seed,
                    w,
                    h,
                    mines,
                    mode
                }),
            (0..Event::SEATS, any::<u8>(), any::<u8>()).prop_map(|(player, x, y)| Event::Reveal {
                player,
                x,
                y
            }),
            (0..Event::SEATS, any::<u8>(), any::<u8>()).prop_map(|(player, x, y)| Event::Flag {
                player,
                x,
                y
            }),
        ]
    }

    fn any_playable_stamped() -> impl Strategy<Value = Stamped> {
        (any::<u32>(), any_playable_event(), any::<u64>()).prop_map(|(seq, ev, at_ms)| Stamped {
            seq,
            ev,
            at_ms,
        })
    }

    /// A board that cannot be dealt is refused before the engine folds it —
    /// `place_mines` used to trap on exactly these, taking the tab with it.
    #[test]
    fn unplayable_events_are_refused_by_decode_msg() {
        let bad = [
            Event::Start {
                seed: 1,
                w: 9,
                h: 9,
                mines: 500,
                mode: Mode::Coop,
            },
            Event::Start {
                seed: 1,
                w: 0,
                h: 9,
                mines: 1,
                mode: Mode::Coop,
            },
            Event::Start {
                seed: 1,
                w: 9,
                h: 0,
                mines: 1,
                mode: Mode::Coop,
            },
            Event::Start {
                seed: 1,
                w: 1,
                h: 1,
                mines: 1,
                mode: Mode::Coop,
            },
            Event::Start {
                seed: 1,
                w: 255,
                h: 255,
                mines: 1,
                mode: Mode::Coop,
            },
            Event::Reveal {
                player: 200,
                x: 0,
                y: 0,
            },
            Event::Flag {
                player: 2,
                x: 0,
                y: 0,
            },
        ];
        for ev in bad {
            assert!(!ev.is_playable(), "{ev:?} passed is_playable");
            let msg = encode_msg(&Msg::Events(vec![stamp(0, ev)]));
            assert_eq!(
                decode_msg::<Event>(&msg),
                None,
                "{ev:?} survived decode_msg"
            );
        }
    }

    /// One bad event poisons its whole message: half a log is how peers
    /// diverge without anyone noticing.
    #[test]
    fn one_unplayable_event_drops_the_whole_message() {
        let good = stamp(
            0,
            Event::Start {
                seed: 1,
                w: 9,
                h: 9,
                mines: 10,
                mode: Mode::Coop,
            },
        );
        let bad = stamp(
            1,
            Event::Reveal {
                player: 9,
                x: 0,
                y: 0,
            },
        );
        assert!(decode_msg::<Event>(&encode_msg(&Msg::Events(vec![good]))).is_some());
        assert_eq!(
            decode_msg::<Event>(&encode_msg(&Msg::Events(vec![good, bad]))),
            None
        );
    }

    fn any_stamped() -> impl Strategy<Value = Stamped> {
        (any::<u32>(), any_event(), any::<u64>()).prop_map(|(seq, ev, at_ms)| Stamped {
            seq,
            ev,
            at_ms,
        })
    }

    fn any_event() -> impl Strategy<Value = Event> {
        prop_oneof![
            (any::<u64>(), any::<u8>(), any::<u8>(), any::<u16>()).prop_map(
                |(seed, w, h, mines)| Event::Start {
                    seed,
                    w,
                    h,
                    mines,
                    mode: Mode::Coop
                }
            ),
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
        fn logs_round_trip(evs in prop::collection::vec(any_stamped(), 0..64)) {
            prop_assert_eq!(decode_log(&encode_log(&evs)), Some(evs));
        }

        /// Whatever order two peers hear about events in, sorting lands them
        /// in the same one. This is the property the merge in the shell leans
        /// on, so it is checked here rather than assumed.
        #[test]
        fn sorting_is_order_independent(
            evs in prop::collection::vec(any_stamped(), 0..32),
            cut in 0usize..32,
        ) {
            let cut = cut.min(evs.len());
            let (a, b) = evs.split_at(cut);
            let mut theirs: Vec<Stamped> = b.iter().chain(a).copied().collect();
            let mut ours = evs.clone();
            ours.sort();
            theirs.sort();
            prop_assert_eq!(ours, theirs);
        }

        /// The trust-boundary test: arbitrary bytes must never panic, only
        /// fail. Anything reachable from a DataChannel needs one of these.
        #[test]
        fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..256)) {
            let _ = decode_log::<Event>(&bytes);
            let _ = decode_msg::<Event>(&bytes);
        }

        #[test]
        fn event_messages_round_trip(evs in prop::collection::vec(any_playable_stamped(), 0..64)) {
            let m = Msg::Events(evs);
            prop_assert_eq!(decode_msg(&encode_msg(&m)), Some(m));
        }
    }
}

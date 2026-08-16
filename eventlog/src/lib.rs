//! A replicated event log for two peers on an unreliable channel.
//!
//! The log is the state: peers exchange stamped events, keep them in one
//! agreed total order, and fold their own domain state from the result. This
//! crate owns the order, the Lamport clock, the merge, and the byte framing;
//! what an event *means* belongs to the application, which plugs in via
//! [`Payload`].
//!
//! Extracted from a P2P Minesweeper, where the hazards were earned one at a
//! time: echoes must not become second moves, a hostile peer must not unsort
//! or overflow the log, and half a message is worse than none. The comments
//! below carry those scars on purpose.

use std::ops::Deref;

/// What the application's events must provide to travel through a log.
///
/// `Ord` is not a formality: the derived order on the payload is what breaks
/// ties between two peers who picked the same `seq`, so both sides sort the
/// same log identically without negotiating.
pub trait Payload: Copy + Ord {
    /// Appends this event's bytes to `out`. The format is the application's
    /// protocol; once two peers have spoken it, changing it means versioning.
    fn encode(&self, out: &mut Vec<u8>);

    /// Decodes one event from the front of `bytes`, returning it and how many
    /// bytes it consumed. `None` means truncated or malformed — never panic,
    /// never guess: the bytes come from a peer.
    fn decode(bytes: &[u8]) -> Option<(Self, usize)>;

    /// Whether this event is one the application could act on. Checked by
    /// [`decode_msg`] on every event in a message, so an impossible event —
    /// however well-formed its bytes — is refused at the trust boundary
    /// instead of being folded into state.
    fn valid(&self) -> bool {
        true
    }
}

/// An event with its place in the total order, and when it happened.
///
/// `seq` is a Lamport clock: one more than the highest any peer has been seen
/// to use. Two peers moving at the same time pick the same `seq`, so the
/// derived `Ord` breaks the tie on the event itself.
///
/// `at_ms` is the author's wall clock, in Unix milliseconds. It is
/// deliberately last in the field order — and therefore last in the derived
/// `Ord` — because it must never decide the order of two events. It is there
/// so time can be read out of the log instead of measured locally, which is
/// what makes a peer who joins in the middle see the same elapsed time as
/// everyone else.
///
/// Field order is the sort order. Do not reorder it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Stamped<P> {
    pub seq: u32,
    pub ev: P,
    pub at_ms: u64,
}

/// Folds a peer's events into `log` and returns whether anything was new.
///
/// The log is kept sorted by `Stamped`'s own order, which both peers agree
/// on, so it does not matter who heard what first. Re-delivery of an event we
/// already have is a no-op rather than a second move.
pub fn merge<P: Copy + Ord>(log: &mut Vec<Stamped<P>>, incoming: &[Stamped<P>]) -> bool {
    let mut changed = false;
    for &s in incoming {
        // An event is identified by its place in the order and what it does —
        // *not* by when it was made. `at_ms` is in `Stamped`'s derived `Ord`,
        // so an echo of one of our own events with a fresh timestamp would
        // sort as a separate entry and apply the same event a second time.
        match log.binary_search_by(|p| (p.seq, p.ev).cmp(&(s.seq, s.ev))) {
            Ok(_) => {}
            Err(i) => {
                log.insert(i, s);
                changed = true;
            }
        }
    }
    changed
}

/// Whether merging `incoming` could push a log past `cap`. Saturating, so a
/// peer claiming an absurd count cannot wrap the sum into "plenty of room".
pub fn overflows(ours: usize, incoming: usize, cap: usize) -> bool {
    ours.saturating_add(incoming) > cap
}

/// A log we are about to adopt wholesale has to be one we could have built
/// ourselves: bounded by `cap`, sorted, free of duplicates, and opening with
/// an event `opens` recognises at seq 0. [`merge`] binary-searches this
/// afterwards, so an unsorted log would quietly disable de-duplication for
/// the rest of the session.
pub fn sanitised<P: Copy + Ord>(
    mut events: Vec<Stamped<P>>,
    cap: usize,
    opens: impl Fn(&P) -> bool,
) -> Option<Vec<Stamped<P>>> {
    if events.len() > cap {
        return None;
    }
    events.sort();
    events.dedup_by(|a, b| (a.seq, a.ev) == (b.seq, b.ev));
    match events.first() {
        Some(s) if s.seq == 0 && opens(&s.ev) => Some(events),
        _ => None,
    }
}

/// The log plus the Lamport clock that stamps additions to it. Owning both
/// keeps the two invariants that are easy to lose in application code: a
/// local event always sorts after everything already seen, and the clock
/// never falls behind a merged-in peer's.
///
/// Derefs to `[Stamped<P>]`, so reading it is reading a slice.
#[derive(Clone, Debug)]
pub struct Log<P> {
    events: Vec<Stamped<P>>,
    clock: u32,
}

impl<P: Copy + Ord> Log<P> {
    /// A fresh log opened by its first event, at seq 0 — the shape
    /// [`sanitised`] demands of any log worth adopting.
    pub fn open(first: P, at_ms: u64) -> Self {
        Log {
            events: vec![Stamped {
                seq: 0,
                ev: first,
                at_ms,
            }],
            clock: 0,
        }
    }

    /// Stamps and records a local event, returning the stamp so the caller
    /// can send it.
    pub fn append(&mut self, ev: P, at_ms: u64) -> Stamped<P> {
        // Saturating: a peer can hand us seq = u32::MAX, and wrapping would
        // put our next event at the very start of the log.
        self.clock = self.clock.saturating_add(1);
        let s = Stamped {
            seq: self.clock,
            ev,
            at_ms,
        };
        // Through `merge`, not `push`: it is the only writer that keeps the
        // log sorted and free of duplicates. A peer can pin our clock at
        // u32::MAX, and an appended event at a seq we already hold would sit
        // out of order — which is exactly what the binary search relies on
        // being true.
        merge(&mut self.events, &[s]);
        s
    }

    /// Folds a peer's events in; returns whether anything was new.
    pub fn merge(&mut self, incoming: &[Stamped<P>]) -> bool {
        let new = merge(&mut self.events, incoming);
        self.catch_up();
        new
    }

    /// Replaces the whole log — a handover of somebody else's game. The
    /// caller decides *whether* (see [`sanitised`]); this keeps the clock
    /// honest afterwards.
    pub fn adopt(&mut self, events: Vec<Stamped<P>>) {
        self.events = events;
        self.catch_up();
    }

    /// Our clock must outrun anything we have now seen, or our next event
    /// would sort before one that has already happened.
    fn catch_up(&mut self) {
        self.clock = self.clock.max(self.events.last().map_or(0, |s| s.seq));
    }
}

impl<P> Deref for Log<P> {
    type Target = [Stamped<P>];
    fn deref(&self) -> &[Stamped<P>] {
        &self.events
    }
}

// ---------------------------------------------------------------------------
// Wire framing.
//
// Hand-rolled, little-endian. A stamped event is `seq(4) at_ms(8)` followed
// by the payload's own bytes; a message is a one-byte envelope. Every decode
// path is a trust boundary — the bytes arrive from a peer, and a peer can be
// buggy, outdated, or hostile. Nothing here panics or indexes unchecked;
// malformed input returns `None` and the caller drops the message.
// ---------------------------------------------------------------------------

const SEQ_LEN: usize = 4;
const AT_LEN: usize = 8;
/// The `seq` + `at_ms` prefix on every record in a log.
pub const STAMP_LEN: usize = SEQ_LEN + AT_LEN;

impl<P: Payload> Stamped<P> {
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.at_ms.to_le_bytes());
        self.ev.encode(out);
    }

    pub fn decode(bytes: &[u8]) -> Option<(Stamped<P>, usize)> {
        let seq = u32::from_le_bytes(bytes.get(..SEQ_LEN)?.try_into().ok()?);
        let at_ms = u64::from_le_bytes(bytes.get(SEQ_LEN..STAMP_LEN)?.try_into().ok()?);
        let (ev, n) = P::decode(&bytes[STAMP_LEN..])?;
        Some((Stamped { seq, ev, at_ms }, STAMP_LEN + n))
    }
}

/// What one message on the channel can be.
///
/// The event stream alone cannot carry a checksum, so messages get a one-byte
/// envelope. Two kinds is all a replicated log needs: events, and "here is
/// what my state looks like now".
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Msg<P> {
    Events(Vec<Stamped<P>>),
    /// The sender's state hash after applying `count` events. Compared only
    /// when the receiver is also at `count` — different counts just mean one
    /// side is behind, which is normal and not a desync.
    State { count: u32, hash: u64 },
}

const MSG_EVENTS: u8 = 0x00;
const MSG_STATE: u8 = 0x01;
pub const STATE_LEN: usize = 13;

pub fn encode_msg<P: Payload>(msg: &Msg<P>) -> Vec<u8> {
    match msg {
        Msg::Events(events) => {
            let mut out = vec![MSG_EVENTS];
            out.extend_from_slice(&encode_log(events));
            out
        }
        Msg::State { count, hash } => {
            let mut out = vec![MSG_STATE];
            out.extend_from_slice(&count.to_le_bytes());
            out.extend_from_slice(&hash.to_le_bytes());
            out
        }
    }
}

/// Same trust rules as [`decode_log`]: a peer wrote these bytes, so anything
/// unexpected returns `None` instead of being partially believed.
pub fn decode_msg<P: Payload>(bytes: &[u8]) -> Option<Msg<P>> {
    match *bytes.first()? {
        // A message is all-or-nothing: one invalid event and the whole thing
        // is dropped, because half a log is how peers diverge quietly.
        MSG_EVENTS => decode_log(&bytes[1..])
            .filter(|log: &Vec<Stamped<P>>| log.iter().all(|s| s.ev.valid()))
            .map(Msg::Events),
        MSG_STATE if bytes.len() == STATE_LEN => Some(Msg::State {
            count: u32::from_le_bytes(bytes[1..5].try_into().ok()?),
            hash: u64::from_le_bytes(bytes[5..13].try_into().ok()?),
        }),
        _ => None,
    }
}

pub fn encode_log<P: Payload>(events: &[Stamped<P>]) -> Vec<u8> {
    let mut out = Vec::new();
    for ev in events {
        ev.encode(&mut out);
    }
    out
}

/// Decodes a whole log. Leftover or truncated bytes are a failure, not a
/// partial success — accepting half a log would desync the peers silently,
/// which is exactly the failure mode this format exists to prevent.
pub fn decode_log<P: Payload>(bytes: &[u8]) -> Option<Vec<Stamped<P>>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let (ev, n) = Stamped::decode(&bytes[i..])?;
        out.push(ev);
        i += n;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The smallest payload with the properties the log cares about: a total
    /// order and a byte format. `valid` refuses one value so the message
    /// boundary is testable.
    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
    struct Tick(u8);

    impl Payload for Tick {
        fn encode(&self, out: &mut Vec<u8>) {
            out.push(self.0);
        }
        fn decode(bytes: &[u8]) -> Option<(Self, usize)> {
            Some((Tick(*bytes.first()?), 1))
        }
        fn valid(&self) -> bool {
            self.0 != 0xFF
        }
    }

    fn at(seq: u32, ev: u8, at_ms: u64) -> Stamped<Tick> {
        Stamped {
            seq,
            ev: Tick(ev),
            at_ms,
        }
    }

    #[test]
    fn peers_converge_whatever_order_events_arrive_in() {
        let (a, b) = (at(1, 10, 0), at(1, 20, 0));
        let mut ours = vec![at(0, 1, 0), a];
        merge(&mut ours, &[b]);
        let mut theirs = vec![at(0, 1, 0), b];
        merge(&mut theirs, &[a]);
        assert_eq!(ours, theirs);
        assert!(ours.windows(2).all(|w| w[0] < w[1]));
    }

    /// A peer can echo one of our own events back with a fresh timestamp. If
    /// the timestamp were part of an event's identity, that echo would land
    /// as a second event — with both peers agreeing on the corrupted state,
    /// so no desync check would ever fire.
    #[test]
    fn an_event_re_stamped_with_a_new_time_is_not_a_second_event() {
        let mut log = vec![at(0, 1, 0), at(1, 5, 1_000)];
        assert!(!merge(&mut log, &[at(1, 5, 9_999)]));
        assert_eq!(log.len(), 2);
        assert_eq!(log[1].at_ms, 1_000, "the original timestamp was replaced");
    }

    #[test]
    fn the_log_owns_its_clock() {
        let mut log = Log::open(Tick(1), 0);
        let s = log.append(Tick(2), 10);
        assert_eq!(s.seq, 1);

        // Hearing a peer far ahead pulls the clock up...
        log.merge(&[at(7, 3, 20)]);
        assert_eq!(log.append(Tick(4), 30).seq, 8);
        // ...and a saturated clock cannot unsort the log.
        log.merge(&[at(u32::MAX, 5, 40)]);
        log.append(Tick(6), 50);
        assert!(log.windows(2).all(|w| w[0] < w[1]));

        // Adoption resets the events but never lets the clock fall behind.
        log.adopt(vec![at(0, 1, 0), at(3, 9, 5)]);
        assert!(log.append(Tick(7), 60).seq > 3);
    }

    #[test]
    fn sanitised_rebuilds_only_a_log_we_could_have_built() {
        let opens = |p: &Tick| p.0 == 1;
        let tidy = sanitised(vec![at(2, 5, 20), at(0, 1, 0), at(1, 3, 10), at(2, 5, 99)], 10, opens)
            .expect("a real log was refused");
        assert_eq!(tidy.len(), 3, "the duplicate survived");
        assert!(tidy.windows(2).all(|w| w[0] < w[1]));

        assert!(sanitised::<Tick>(vec![], 10, opens).is_none());
        assert!(sanitised(vec![at(1, 3, 0)], 10, opens).is_none(), "no opening");
        assert!(sanitised(vec![at(0, 1, 0); 11], 10, opens).is_none(), "past the cap");
    }

    #[test]
    fn the_cap_refuses_what_would_overflow_it() {
        assert!(!overflows(0, 10, 10));
        assert!(overflows(0, 11, 10));
        assert!(overflows(10, 1, 10));
        assert!(overflows(usize::MAX, usize::MAX, 10));
    }

    #[test]
    fn framing_round_trips_and_rejects_partials() {
        let log = vec![at(0, 1, 7), at(1, 2, 8)];
        let bytes = encode_log(&log);
        assert_eq!(decode_log::<Tick>(&bytes), Some(log.clone()));
        // Cutting inside the second record: the first still decodes, the log
        // as a whole must not.
        for cut in STAMP_LEN + 2..bytes.len() {
            assert!(decode_log::<Tick>(&bytes[..cut]).is_none(), "accepted {cut} bytes");
        }

        let m = Msg::Events(log);
        assert_eq!(decode_msg::<Tick>(&encode_msg(&m)), Some(m));
        let s = Msg::State { count: 7, hash: 0x0102_0304_0506_0708 };
        let sb = encode_msg::<Tick>(&s);
        assert_eq!(sb.len(), STATE_LEN);
        assert_eq!(decode_msg::<Tick>(&sb), Some(s));
        assert!(decode_msg::<Tick>(&sb[..STATE_LEN - 1]).is_none());
        assert!(decode_msg::<Tick>(&[0x7F]).is_none());
    }

    /// One invalid event poisons its whole message: half a log is how peers
    /// diverge without anyone noticing.
    #[test]
    fn an_invalid_event_drops_the_whole_message() {
        let good = Msg::Events(vec![at(0, 1, 0)]);
        assert!(decode_msg::<Tick>(&encode_msg(&good)).is_some());
        let bad = Msg::Events(vec![at(0, 1, 0), at(1, 0xFF, 0)]);
        assert_eq!(decode_msg::<Tick>(&encode_msg(&bad)), None);
    }
}

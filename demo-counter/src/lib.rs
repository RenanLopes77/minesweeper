//! The second consumer — and the reason it exists.
//!
//! A shared tally: two peers, one button each, both screens agree on the
//! count. Everything interesting comes from the two extracted crates:
//! `eventlog` keeps the two of them folding the same ordered log, and
//! `p2p-link` turns two pasted links into the DataChannel the log rides on.
//! What is left — this file — is the application: a payload type, a fold,
//! and a page. If this stays small, the extraction worked.

use std::cell::RefCell;
use std::rc::Rc;

use eventlog::{Log, Msg, Payload, Stamped, decode_msg, encode_msg};
use p2p_link::{Hooks, LinkKind, Role, Session, net, session};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::HtmlTextAreaElement;

/// The whole protocol: a log opens, and people tap.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Ev {
    Open,
    Tap { player: u8 },
}

const TAG_OPEN: u8 = 0;
const TAG_TAP: u8 = 1;

impl Payload for Ev {
    fn encode(&self, out: &mut Vec<u8>) {
        match *self {
            Ev::Open => out.push(TAG_OPEN),
            Ev::Tap { player } => out.extend([TAG_TAP, player]),
        }
    }
    fn decode(bytes: &[u8]) -> Option<(Self, usize)> {
        match *bytes.first()? {
            TAG_OPEN => Some((Ev::Open, 1)),
            TAG_TAP => Some((
                Ev::Tap {
                    player: *bytes.get(1)?,
                },
                2,
            )),
            _ => None,
        }
    }
    fn valid(&self) -> bool {
        match *self {
            Ev::Open => true,
            Ev::Tap { player } => player < 2,
        }
    }
}

/// Far past any thumb, same purpose as the game's cap: a peer inventing
/// events forever must not grow the log — and the refold — without bound.
const MAX_LOG: usize = 10_000;

struct App {
    log: Log<Ev>,
    chan: Option<web_sys::RtcDataChannel>,
    player: u8,
}

type Shared = Rc<RefCell<App>>;

fn tallies(log: &[Stamped<Ev>]) -> [u32; 2] {
    let mut t = [0u32; 2];
    for s in log {
        if let Ev::Tap { player } = s.ev {
            t[player as usize % 2] += 1;
        }
    }
    t
}

fn render(app: &App) {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("tally"))
    {
        let [a, b] = tallies(&app.log);
        let me = app.player as usize;
        el.set_text_content(Some(&format!(
            "you {} — {} them   ({} total)",
            [a, b][me],
            [a, b][1 - me],
            a + b
        )));
    }
}

fn send(app: &App, msg: &Msg<Ev>) {
    if let Some(ch) = app
        .chan
        .as_ref()
        .filter(|c| c.ready_state() == web_sys::RtcDataChannelState::Open)
    {
        let _ = ch.send_with_u8_array(&encode_msg(msg));
    }
}

fn tap(app: &Shared) {
    let mut a = app.borrow_mut();
    let player = a.player;
    let s = a.log.append(Ev::Tap { player }, js_sys::Date::now() as u64);
    send(&a, &Msg::Events(vec![s]));
    render(&a);
}

/// Bytes from the peer — the same trust boundary as the game's: malformed or
/// oversized input is dropped whole, never half-believed.
fn remote(app: &Shared, bytes: &[u8]) {
    let Some(Msg::Events(events)) = decode_msg::<Ev>(bytes) else {
        return; // this demo never sends Msg::State, and junk is junk
    };
    let mut a = app.borrow_mut();
    if eventlog::overflows(a.log.len(), events.len(), MAX_LOG) {
        return net::note("ignored a message past the log cap");
    }
    a.log.merge(&events);
    render(&a);
}

fn text_of(id: &str) -> Option<HtmlTextAreaElement> {
    web_sys::window()?
        .document()?
        .get_element_by_id(id)?
        .dyn_into()
        .ok()
}

fn hooks(app: Shared) -> Hooks {
    Hooks {
        on_role: {
            let app = app.clone();
            Box::new(move |is_host| app.borrow_mut().player = u8::from(!is_host))
        },
        on_link: Box::new(|kind, link| {
            if let Some(ta) = text_of("sig") {
                ta.set_value(link);
                ta.select();
            }
            net::note(match kind {
                LinkKind::Offer => "send this link to the other side; paste their reply below",
                LinkKind::Answer => "send this reply back to the host",
            });
        }),
        on_channel: {
            let app = app.clone();
            Box::new(move |ch, _| app.borrow_mut().chan = Some(ch))
        },
        on_open: {
            let app = app.clone();
            Box::new(move |_| {
                net::note("connected — tap away");
                // Both sides hand over the whole log: on a fresh join that is
                // the catch-up, and merging makes re-delivery harmless.
                let a = app.borrow();
                send(&a, &Msg::Events(a.log.to_vec()));
                render(&a);
            })
        },
        on_message: {
            let app = app.clone();
            Box::new(move |bytes| remote(&app, &bytes))
        },
        on_drop: Box::new(move || {
            app.borrow_mut().chan = None;
            net::note("connection lost — the tally is kept; host or join again");
        }),
        on_reset: Box::new(|| {
            for id in ["sig", "reply"] {
                if let Some(ta) = text_of(id) {
                    ta.set_value("");
                }
            }
        }),
    }
}

#[wasm_bindgen(start)]
pub fn main() -> Result<(), JsValue> {
    let doc = web_sys::window()
        .ok_or("no window")?
        .document()
        .ok_or("no document")?;

    net::on_note(|s| {
        if let Some(el) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id("status"))
        {
            el.set_text_content(Some(s));
        }
    });

    let app: Shared = Rc::new(RefCell::new(App {
        log: Log::open(Ev::Open, js_sys::Date::now() as u64),
        chan: None,
        player: 0,
    }));
    render(&app.borrow());

    let sess = Session::new(hooks(app.clone()));

    let on = |id: &str, cb: Closure<dyn FnMut()>| -> Result<(), JsValue> {
        doc.get_element_by_id(id)
            .ok_or_else(|| JsValue::from_str(&format!("no #{id}")))?
            .add_event_listener_with_callback("click", cb.as_ref().unchecked_ref())?;
        cb.forget();
        Ok(())
    };

    {
        let app = app.clone();
        on("tap", Closure::new(move || tap(&app)))?;
    }
    {
        let sess = sess.clone();
        on("go", Closure::new(move || sess.host()))?;
    }
    {
        let sess = sess.clone();
        on(
            "connect",
            Closure::new(move || {
                if let Some(ta) = text_of("reply") {
                    sess.accept(&ta.value(), true);
                }
            }),
        )?;
    }
    // A paste into the box connects by itself, same as the game.
    {
        let sess = sess.clone();
        let ta = text_of("reply").ok_or("no #reply")?;
        let src = ta.clone();
        let cb = Closure::<dyn FnMut()>::new(move || sess.accept(&src.value(), false));
        ta.add_event_listener_with_callback("input", cb.as_ref().unchecked_ref())?;
        cb.forget();
    }
    // Opened from a host's link: answer without being asked.
    if session::url_has_offer() {
        net::note("opened from a link — answering…");
        sess.join_url_offer();
    }
    if sess.role() == Role::Idle {
        net::note("press Host, or open somebody's link");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The codec is the only logic here that can break quietly; everything
    /// else is the two crates under their own tests.
    #[test]
    fn the_payload_round_trips_and_refuses_junk() {
        for ev in [Ev::Open, Ev::Tap { player: 1 }] {
            let mut v = Vec::new();
            ev.encode(&mut v);
            assert_eq!(Ev::decode(&v), Some((ev, v.len())));
        }
        assert_eq!(Ev::decode(&[9]), None);
        assert_eq!(Ev::decode(&[TAG_TAP]), None, "a tap needs its player");
        assert!(!Ev::Tap { player: 7 }.valid(), "a third seat is not a seat");

        // And two peers tapping at once agree on one tally.
        let mut ours = Log::open(Ev::Open, 0);
        let mine = ours.append(Ev::Tap { player: 0 }, 1);
        let mut theirs = Log::open(Ev::Open, 0);
        let their_tap = theirs.append(Ev::Tap { player: 1 }, 1);
        ours.merge(&[their_tap]);
        theirs.merge(&[mine]);
        assert_eq!(tallies(&ours), [1, 1]);
        assert_eq!(tallies(&ours), tallies(&theirs));
    }
}

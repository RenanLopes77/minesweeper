//! Signalling by link, headless.
//!
//! The host calls [`Session::host`] and gets a URL through [`Hooks::on_link`].
//! Anything that can carry a link can carry it — chat, email, a QR code
//! someone photographs. Opening that link auto-joins: the page sees the offer
//! in its own address bar ([`Session::join_url_offer`]), answers it, and
//! hands back a second link. The host pastes that one in
//! ([`Session::accept`]) and they are connected. Two links, no server.
//!
//! The SDP rides in the URL **fragment**. Fragments are never sent to the
//! server, so even on static hosting the handshake stays between the two
//! peers.
//!
//! Everything a page would show — where the link goes, what "connected"
//! looks like, what to say when the peer vanishes — arrives through
//! [`Hooks`]; this module owns only the state machine and the wire.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::{RtcDataChannel, RtcDataChannelType, RtcPeerConnection};

use crate::{b64, net, sdp, zip};

/// Who we are in the handshake. Decides what a pasted link means.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// Nothing started. Pasted text is an offer to answer.
    Idle,
    /// We made the offer. Pasted text is the answer that completes it.
    Host,
    /// Our side of the handshake is done. Further pastes mean nothing.
    Done,
}

/// Which of the two links a [`Hooks::on_link`] call carries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LinkKind {
    /// The host's invite. Send it to the peer; their reply comes back.
    Offer,
    /// The joiner's reply. Send it back to the host and the channel opens.
    Answer,
}

/// See [`Hooks::on_link`]. A named type only because clippy balks at the
/// nested one inline.
pub type LinkFn = Box<dyn Fn(LinkKind, &str)>;

/// How the application plugs into the handshake. Every hook is UI- or
/// state-shaped; the session never touches the page itself.
///
/// Progress text goes through [`net::on_note`]/[`net::on_log`] instead — the
/// session narrates the mechanics ("gathering candidates…"), the application
/// narrates what they mean for its user.
pub struct Hooks {
    /// A handshake is starting and this side's role is now known. Called
    /// before the offer or answer is made: the peer's first bytes can arrive
    /// before `on_open`, and the application may need its identity settled
    /// by then.
    pub on_role: Box<dyn Fn(bool)>,
    /// Our link is ready. Display it, copy it, QR it — it is the one thing
    /// the human has to carry.
    pub on_link: LinkFn,
    /// The DataChannel exists (not necessarily open yet — see `on_open`).
    /// Store it; it is what `send` happens on. Binary type is already
    /// arraybuffer.
    pub on_channel: Box<dyn Fn(RtcDataChannel, bool)>,
    /// The channel opened; the argument is whether we hosted. Both sides get
    /// here — the host from its own channel, the joiner from `ondatachannel`.
    pub on_open: Box<dyn Fn(bool)>,
    /// Bytes from the peer.
    pub on_message: Box<dyn Fn(Vec<u8>)>,
    /// A channel that had been handed over is gone — the network vanished or
    /// the peer hung up. The session has already reset itself (`on_reset`
    /// follows); drop your copy of the channel and tell the user.
    pub on_drop: Box<dyn Fn()>,
    /// The handshake is back to the state the page loads in. Clear whatever
    /// `on_link` displayed.
    pub on_reset: Box<dyn Fn()>,
}

struct Inner {
    /// Owns the connection for the life of the session; the application owns
    /// the channel (see [`Hooks::on_channel`]).
    pc: RefCell<Option<RtcPeerConnection>>,
    role: Cell<Role>,
    /// Which handshake is current. A connection we have walked away from
    /// keeps reporting for a while — a channel closing, ICE giving up — and
    /// those reports used to tear down whatever session had started since.
    /// Every flow remembers the generation it belongs to and stays quiet
    /// once it is stale.
    era: Cell<u32>,
    /// Whether a channel has been handed to the application this era. What
    /// separates "the network died under us" from "ICE gave up while a human
    /// was still carrying the reply" — see `dropped`.
    has_chan: Cell<bool>,
    hooks: Hooks,
}

/// One two-peer handshake at a time, restartable forever. Clone is a handle,
/// not a copy.
#[derive(Clone)]
pub struct Session {
    inner: Rc<Inner>,
}

/// `#o=` for an offer, `#a=` for an answer. Compacted to its bare facts when
/// the SDP is one we fully understand (see `sdp.rs`), deflated whole when it
/// is not — either way this is most of the difference between a QR code that
/// scans and one that does not.
async fn make_link(kind: char, sdp_text: &str) -> String {
    let packed = match sdp::compact(sdp_text) {
        Some(bytes) => bytes,
        None => match zip::deflate(sdp_text.as_bytes()).await {
            Ok(bytes) => bytes,
            // Compression is an optimisation, never a requirement: a peer
            // that cannot deflate still gets a working, longer link.
            Err(e) => {
                net::log(&format!("deflate unavailable, sending plain: {e:?}"));
                sdp_text.as_bytes().to_vec()
            }
        },
    };
    net::log(&format!(
        "sdp {} bytes -> {} on the wire",
        sdp_text.len(),
        packed.len()
    ));
    link_of(kind, &packed)
}

fn link_of(kind: char, payload: &[u8]) -> String {
    let base = web_sys::window()
        .map(|w| {
            let l = w.location();
            format!(
                "{}{}",
                l.origin().unwrap_or_default(),
                l.pathname().unwrap_or_default()
            )
        })
        .unwrap_or_default();
    format!("{base}#{kind}={}", b64::encode(payload))
}

/// Caps on what a paste may decode to. A real deflated SDP is under a
/// kilobyte and inflates to a couple; anything past these is not a link, it
/// is a decompression bomb — deflate expands up to ~1000:1, and the inflated
/// bytes are allocated before anyone can look at them. Capping the input
/// bounds that allocation; capping the output rejects what still slips by.
const MAX_PACKED: usize = 16 * 1024;
const MAX_SDP: usize = 64 * 1024;

/// What a pasted string turned out to be.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Pasted {
    /// Someone pasted the SDP itself. Nothing to decode.
    Sdp(String),
    /// Bytes out of a link. Deflated, unless it came from an older peer.
    Packed(Vec<u8>),
}

/// Accepts whatever the user pasted: a whole link, a bare `#a=…` fragment,
/// raw base64, or the SDP itself. People paste all four.
///
/// Kept free of the decompression so it stays a pure function that the native
/// tests can reach — `CompressionStream` only exists in a browser.
fn shape_of(s: &str) -> Option<Pasted> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    // Already an SDP: hand it back untouched. Trimming here is what ate the
    // final line's terminator once already — net::normalize_sdp owns cleanup.
    if t.starts_with("v=") {
        return Some(Pasted::Sdp(s.to_string()));
    }
    let payload = match t.rfind(['#', '=']) {
        Some(i) => &t[i + 1..],
        None => t,
    };
    // 4/3 is base64's expansion, so the cap lands on the decoded bytes —
    // and checking the text first means a bomb is refused before it is
    // even decoded.
    if payload.is_empty() || payload.len() > MAX_PACKED * 4 / 3 + 4 {
        return None;
    }
    Some(Pasted::Packed(b64::decode(payload)?))
}

/// The SDP a paste carries: expanded from the compact form, or inflated if
/// it is a deflated one from an older peer.
async fn sdp_from_input(s: &str) -> Option<String> {
    match shape_of(s)? {
        Pasted::Sdp(sdp_text) => Some(sdp_text),
        Pasted::Packed(bytes) => {
            if let Some(sdp_text) = sdp::expand(&bytes) {
                return Some(sdp_text);
            }
            unpack_deflated(bytes).await
        }
    }
}

/// The pre-compact formats: deflated SDP, or — older still — plain base64.
async fn unpack_deflated(bytes: Vec<u8>) -> Option<String> {
    match zip::inflate(&bytes).await {
        // The output cap: input small enough to pass `shape_of` can
        // still inflate to far more than any SDP has business being.
        Ok(plain) if plain.len() <= MAX_SDP => String::from_utf8(plain).ok(),
        Ok(_) => None,
        // Links made before the format was compressed are plain base64,
        // and a stray paste can be anything at all. Both land here.
        Err(_) => String::from_utf8(bytes)
            .ok()
            .filter(|s| s.starts_with("v=")),
    }
}

/// Is there an offer in the page's own address bar? Cheap and synchronous,
/// so the decision to start a join can be made before the async decode.
pub fn url_has_offer() -> bool {
    web_sys::window()
        .and_then(|w| w.location().hash().ok())
        .is_some_and(|h| h.starts_with("#o="))
}

/// The offer carried in the address bar. Goes through the same decode as a
/// paste, which is what makes a compressed link work in the address bar as
/// well as in a box.
async fn offer_from_url() -> Option<String> {
    let hash = web_sys::window()?.location().hash().ok()?;
    if !hash.starts_with("#o=") {
        return None;
    }
    sdp_from_input(&hash).await
}

impl Session {
    pub fn new(hooks: Hooks) -> Session {
        Session {
            inner: Rc::new(Inner {
                pc: RefCell::new(None),
                role: Cell::new(Role::Idle),
                era: Cell::new(0),
                has_chan: Cell::new(false),
                hooks,
            }),
        }
    }

    pub fn role(&self) -> Role {
        self.inner.role.get()
    }

    /// Whether `text` could be a link or SDP at all — the cheap synchronous
    /// check, for callers that want to probe (a clipboard read, say) without
    /// starting a flow.
    pub fn looks_like_link(text: &str) -> bool {
        shape_of(text).is_some()
    }

    /// Starts hosting: makes the offer, reports the invite via `on_link`.
    /// No-op unless the session is idle.
    pub fn host(&self) {
        if self.inner.role.get() != Role::Idle {
            return;
        }
        let s = self.clone();
        spawn_local(async move { s.host_flow().await });
    }

    /// Acts on a link or SDP from anywhere — a box, the clipboard, a paste.
    ///
    /// `complain` is for the paths where the user pressed something and
    /// deserves an answer. An input box firing on every keystroke should
    /// pass `false`, so half-typed text fails in silence.
    pub fn accept(&self, text: &str, complain: bool) {
        let was = self.inner.role.get();
        if was == Role::Done {
            return;
        }
        // Bail before the async hop on anything that is obviously not a
        // link, so typing in a box does not queue work on every character.
        if shape_of(text).is_none() {
            if complain {
                net::note("that does not look like a link — paste the whole thing");
            }
            return;
        }
        let s = self.clone();
        let text = text.to_string();
        spawn_local(async move {
            let Some(sdp_text) = sdp_from_input(&text).await else {
                if complain {
                    net::note("that link did not decode — copy it again, all of it");
                }
                return;
            };
            // Re-check: decoding took a turn of the event loop, and a second
            // paste may have got here first.
            if s.inner.role.get() == Role::Done {
                return;
            }
            s.inner.role.set(Role::Done);
            match was {
                Role::Host => s.finish_host(&sdp_text).await,
                _ => s.join_flow(sdp_text).await,
            }
        });
    }

    /// Answers the offer in the page's own address bar, if there is one and
    /// the session has not already finished a handshake. The caller wires
    /// this to page load and `hashchange`; a link that arrives while the tab
    /// is already open only changes the fragment, so load-time code alone
    /// would never see it.
    pub fn join_url_offer(&self) {
        if self.inner.role.get() == Role::Done || !url_has_offer() {
            return;
        }
        // Claimed before the async decode, so a second trigger cannot start
        // a second join.
        self.inner.role.set(Role::Done);
        let s = self.clone();
        spawn_local(async move {
            match offer_from_url().await {
                Some(offer) => s.join_flow(offer).await,
                None => {
                    // Let them try again rather than sitting in Done.
                    s.inner.role.set(Role::Idle);
                    net::note("that link did not decode — copy it again, all of it");
                }
            }
        });
    }

    /// Back to the state the page loads in. Whatever the application keeps —
    /// its own state survives a dropped channel — reconnecting is a fresh
    /// handshake over that old state.
    pub fn reset(&self) {
        let i = &self.inner;
        i.era.set(i.era.get().wrapping_add(1));
        // Close it, do not merely forget it. An abandoned connection keeps
        // negotiating and reports Failed minutes later — into whatever
        // session has started since, tearing that one down instead.
        if let Some(conn) = i.pc.borrow().as_ref() {
            conn.close();
        }
        *i.pc.borrow_mut() = None;
        i.role.set(Role::Idle);
        i.has_chan.set(false);
        (i.hooks.on_reset)();
    }

    /// The channel is gone. Called from the channel's own `onclose` and from
    /// the connection state machine, whichever notices first — hence the era
    /// guard: a report from a handshake we have already left says nothing
    /// about the one we are in now.
    fn dropped(&self, mine: u32) {
        let i = &self.inner;
        if i.era.get() != mine {
            return;
        }
        // No channel has ever been handed over: ICE ran out of patience
        // while a human was still carrying the reply, and Chrome calls that
        // Failed. It revives by itself when the answer lands and real checks
        // begin — watched happen over 5G — so say what the wait actually is
        // instead of tearing the handshake down. On a network that truly
        // cannot connect this reads hopeful for a while, but the old silence
        // read broken immediately.
        if !i.has_chan.get() {
            net::note(
                "not connected yet — send the reply if you have not; it connects by itself once the host pastes it",
            );
            return;
        }
        (i.hooks.on_drop)();
        self.reset();
    }

    fn on_drop_cb(&self, mine: u32) -> Rc<dyn Fn()> {
        let s = self.clone();
        Rc::new(move || s.dropped(mine))
    }

    async fn host_flow(&self) {
        let i = &self.inner;
        let mine = i.era.get();
        net::note("gathering candidates…");
        let conn = match net::new_connection() {
            Ok(c) => c,
            Err(e) => return net::note(&format!("connection failed: {e:?}")),
        };
        // The role is settled now, not when the channel opens: the peer's
        // first bytes can arrive before `on_open` resolves, and the
        // application may decide what they mean by asking who it is.
        (i.hooks.on_role)(true);
        net::trace_states(&conn, "host", self.on_drop_cb(mine));
        match net::make_offer(&conn).await {
            Ok((ch, sdp_text)) => {
                self.watch(ch, true, mine);
                let link = make_link('o', &sdp_text).await;
                *i.pc.borrow_mut() = Some(conn);
                i.role.set(Role::Host);
                net::log(&format!(
                    "[host] candidates: {} ({} chars)",
                    net::candidate_summary(&sdp_text),
                    link.len()
                ));
                (i.hooks.on_link)(LinkKind::Offer, &link);
            }
            Err(e) => {
                i.role.set(Role::Idle);
                net::note(&format!(
                    "could not make a link ({e:?}) — press Host to try again"
                ));
            }
        }
    }

    async fn join_flow(&self, offer: String) {
        let i = &self.inner;
        let mine = i.era.get();
        net::note("gathering candidates…");
        let conn = match net::new_connection() {
            Ok(c) => c,
            Err(e) => return net::note(&format!("connection failed: {e:?}")),
        };
        // Same reason as in `host_flow`: the host's first bytes may beat
        // `on_open`.
        (i.hooks.on_role)(false);
        net::trace_states(&conn, "join", self.on_drop_cb(mine));
        net::log(&format!(
            "[join] their candidates: {}",
            net::candidate_summary(&offer)
        ));

        // Installed before the answer is created — the channel can arrive
        // the moment the connection completes.
        let incoming = net::on_data_channel(&conn);
        {
            let s = self.clone();
            spawn_local(async move {
                if let Ok(Ok(ch)) = incoming.await.map(|v| v.dyn_into::<RtcDataChannel>()) {
                    s.watch(ch, false, mine);
                }
            });
        }

        match net::accept_offer(&conn, &offer).await {
            Ok(answer) => {
                let link = make_link('a', &answer).await;
                *i.pc.borrow_mut() = Some(conn);
                net::log(&format!(
                    "[join] candidates: {}",
                    net::candidate_summary(&answer)
                ));
                (i.hooks.on_link)(LinkKind::Answer, &link);
            }
            Err(e) => {
                // Back to Idle, or a mistyped link would be the end of the
                // session: every later paste is swallowed by the Done guard.
                i.role.set(Role::Idle);
                net::note(&format!(
                    "that link did not work ({e:?}) — ask for a fresh one"
                ));
            }
        }
    }

    async fn finish_host(&self, answer: &str) {
        let conn = self.inner.pc.borrow().clone();
        let Some(conn) = conn else {
            self.inner.role.set(Role::Idle);
            return net::note("no connection to answer — press Host to start again");
        };
        match net::accept_answer(&conn, answer).await {
            Ok(()) => net::note("reply accepted — connecting…"),
            Err(e) => {
                // The connection is spent once a bad answer has been applied
                // to it, so "paste it again" would be a lie — the old link is
                // dead with it. Back to a clean slate and a fresh link.
                net::log(&format!("bad reply: {e:?}"));
                self.reset();
                net::note("that reply did not work — press Host for a new link");
            }
        }
    }

    /// Hands the channel over and reports when it opens. Both sides end up
    /// here — the host from `create_data_channel`, the joiner from
    /// `ondatachannel`.
    fn watch(&self, ch: RtcDataChannel, is_host: bool, mine: u32) {
        let i = &self.inner;
        let tag = if is_host { "host" } else { "join" };
        net::log(&format!("[{tag}] channel {:?}", ch.ready_state()));

        // Default binaryType is "blob", which would hand us a Blob needing
        // an async read. Arraybuffer is synchronous and is what byte
        // protocols encode to.
        ch.set_binary_type(RtcDataChannelType::Arraybuffer);

        {
            let s = self.clone();
            let on_msg = wasm_bindgen::closure::Closure::<dyn FnMut(web_sys::MessageEvent)>::new(
                move |e: web_sys::MessageEvent| {
                    let bytes = js_sys::Uint8Array::new(&e.data()).to_vec();
                    (s.inner.hooks.on_message)(bytes);
                },
            );
            ch.set_onmessage(Some(on_msg.as_ref().unchecked_ref()));
            on_msg.forget();
        }

        // A drop is not the end of the application's state: that survives,
        // the handshake comes back, and reconnecting is the caller's
        // problem to make cheap.
        {
            let s = self.clone();
            let on_close =
                wasm_bindgen::closure::Closure::<dyn FnMut()>::new(move || s.dropped(mine));
            ch.set_onclose(Some(on_close.as_ref().unchecked_ref()));
            on_close.forget();
        }

        let opened = net::on_open(&ch);
        i.has_chan.set(true);
        (i.hooks.on_channel)(ch, is_host);
        let s = self.clone();
        spawn_local(async move {
            match opened.await {
                Ok(_) => (s.inner.hooks.on_open)(is_host),
                Err(e) => net::note(&format!("[{tag}] channel failed: {e:?}")),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The decompression itself needs a browser, so what is checked here is
    /// the part that decides *what a paste is* — every shape a person might
    /// hand us has to come out as the same payload.
    #[test]
    fn input_accepts_every_shape_a_user_might_paste() {
        let sdp_text = "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\n";
        let packed = b"not really deflate, but opaque bytes either way";
        let b64_text = b64::encode(packed);
        let want = Pasted::Packed(packed.to_vec());

        // The SDP itself is passed through untouched, terminator and all.
        assert_eq!(shape_of(sdp_text), Some(Pasted::Sdp(sdp_text.to_string())));
        assert_eq!(shape_of(&b64_text), Some(want.clone()));
        assert_eq!(shape_of(&format!("#a={b64_text}")), Some(want.clone()));
        assert_eq!(
            shape_of(&format!("  https://x.dev/mine/#o={b64_text}  ")),
            Some(want)
        );
    }

    #[test]
    fn input_rejects_junk() {
        assert!(shape_of("").is_none());
        assert!(shape_of("hello world").is_none());
    }

    /// A paste too big to be a link is a decompression bomb's delivery, and
    /// it has to be refused before its bytes are decoded, let alone inflated.
    #[test]
    fn a_paste_past_the_cap_is_refused() {
        let big = "A".repeat(MAX_PACKED * 2);
        assert!(shape_of(&big).is_none());
        assert!(shape_of(&format!("#o={big}")).is_none());
        // ...while the biggest link the cap allows still decodes.
        let fits = "A".repeat(MAX_PACKED / 4 * 4);
        assert!(shape_of(&fits).is_some());
    }
}

//! Signalling by link.
//!
//! The host presses Host and gets a URL. Anything that can carry a link can
//! carry it — chat, email, a QR code someone photographs. Opening that link
//! auto-joins: the page sees the offer in its own address bar, answers it,
//! and hands back a second link. The host pastes that one in and they are
//! connected. Two links, no server.
//!
//! The SDP rides in the URL **fragment**. Fragments are never sent to the
//! server, so even on GitHub Pages the handshake stays between the two peers.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{
    Document, HtmlTextAreaElement, RtcDataChannel, RtcDataChannelType, RtcPeerConnection,
};

use crate::{app, b64, net, zip};

/// Who we are in the handshake. Decides what a pasted link means.
#[derive(Clone, Copy, PartialEq)]
enum Role {
    /// Nothing started. Pasted text is an offer to answer.
    Idle,
    /// We made the offer. Pasted text is the answer that completes it.
    Host,
    /// Our side of the handshake is done. Further pastes mean nothing.
    Done,
}

type Pc = Rc<RefCell<Option<RtcPeerConnection>>>;

/// Which handshake is current. A connection we have walked away from keeps
/// reporting for a while — a channel closing, ICE giving up — and those
/// reports used to tear down whatever session had started since. Every flow
/// remembers the generation it belongs to and stays quiet once it is stale.
type Era = Rc<Cell<u32>>;

/// `#o=` for an offer, `#a=` for an answer. The SDP is deflated first — it is
/// repetitive ASCII, so this is most of the difference between a QR code that
/// scans and one that does not.
async fn make_link(kind: char, sdp: &str) -> String {
    let packed = match zip::deflate(sdp.as_bytes()).await {
        Ok(bytes) => bytes,
        // Compression is an optimisation, never a requirement: a peer that
        // cannot deflate still gets a working, longer link.
        Err(e) => {
            net::log(&format!("deflate unavailable, sending plain: {e:?}"));
            sdp.as_bytes().to_vec()
        }
    };
    net::log(&format!(
        "sdp {} bytes -> {} on the wire",
        sdp.len(),
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

/// The SDP a paste carries, inflating it if it is compressed.
async fn sdp_from_input(s: &str) -> Option<String> {
    match shape_of(s)? {
        Pasted::Sdp(sdp) => Some(sdp),
        Pasted::Packed(bytes) => match zip::inflate(&bytes).await {
            // The output cap: input small enough to pass `shape_of` can
            // still inflate to far more than any SDP has business being.
            Ok(plain) if plain.len() <= MAX_SDP => String::from_utf8(plain).ok(),
            Ok(_) => None,
            // Links made before the format was compressed are plain base64,
            // and a stray paste can be anything at all. Both land here.
            Err(_) => String::from_utf8(bytes)
                .ok()
                .filter(|s| s.starts_with("v=")),
        },
    }
}

/// Draws the link as a QR code, or hides the canvas if it cannot be drawn.
/// Failure here is cosmetic — the link itself still works — so it is logged
/// rather than surfaced as an error.
fn show_qr(link: &str) {
    let Some(canvas) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("qr"))
        .and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok())
    else {
        return;
    };
    match crate::qr::render(&canvas, link) {
        Ok(modules) => {
            net::log(&format!("qr: {modules}x{modules} modules"));
            let _ = canvas
                .unchecked_ref::<web_sys::HtmlElement>()
                .style()
                .set_property("display", "block");
        }
        Err(e) => net::log(&format!("qr skipped: {e:?}")),
    }
}

/// Selects the box and puts it on the clipboard. `select()` alone is enough on
/// a desktop; on a phone there is no convenient Ctrl+C.
fn copy(ta: &HtmlTextAreaElement) {
    ta.select();
    if let Some(win) = web_sys::window() {
        let p = win.navigator().clipboard().write_text(&ta.value());
        // The browser refuses a clipboard write from a tab that is not
        // focused — which is exactly a joiner answering in a background tab.
        // The link is in the box either way, so this is a note, not a crash;
        // left unhandled it is an uncaught rejection in the console.
        let refused = Closure::<dyn FnMut(JsValue)>::new(|_: JsValue| {
            net::log("clipboard refused — the link is in the box, copy it by hand");
        });
        let _ = p.catch(&refused);
        refused.forget();
    }
}

/// The one button's job changes as the handshake progresses.
fn set_go(label: &str) {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("go"))
    {
        el.set_text_content(Some(label));
    }
}

/// Used to drop the Join button once a handshake is under way, reveal the
/// reply box when the host needs it, and hide the whole panel once connected.
fn display(id: &str, value: &str) {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(id))
        .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
    {
        let _ = el.style().set_property("display", value);
    }
}

/// Back to the state the page loads in, minus the game itself: the board and
/// its log survive a dropped channel, so reconnecting is a fresh handshake
/// over an old game.
fn reset_handshake(pc: &Pc, role: &Rc<Cell<Role>>, era: &Era) {
    era.set(era.get().wrapping_add(1));
    // Close it, do not merely forget it. An abandoned connection keeps
    // negotiating and reports Failed minutes later — into whatever session
    // has started since, tearing that one down instead.
    if let Some(conn) = pc.borrow().as_ref() {
        conn.close();
    }
    *pc.borrow_mut() = None;
    role.set(Role::Idle);
    for id in ["sig", "reply"] {
        if let Some(ta) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id(id))
            .and_then(|e| e.dyn_into::<HtmlTextAreaElement>().ok())
        {
            ta.set_value("");
        }
    }
    display("handshake", "grid");
    display("join", "block");
    display("replybox", "none");
    display("qr", "none");
    set_go("Host a game");
}

/// The channel is gone. Called from the channel's own `onclose` and from the
/// connection state machine, whichever notices first — hence the guard.
fn dropped(app: &app::Shared, pc: &Pc, role: &Rc<Cell<Role>>, era: &Era, mine: u32) {
    // A report from a handshake we have already left says nothing about the
    // one we are in now.
    if era.get() != mine || app.borrow().chan.is_none() {
        return;
    }
    app.borrow_mut().chan = None;
    reset_handshake(pc, role, era);
    net::note("connection lost — your board is kept. Host again, or paste a new link.");
}

/// `dropped` as something the state machine can hold on to.
fn on_drop(
    app: &app::Shared,
    pc: &Pc,
    role: &Rc<Cell<Role>>,
    era: &Era,
    mine: u32,
) -> Rc<dyn Fn()> {
    let (app, pc, role, era) = (app.clone(), pc.clone(), role.clone(), era.clone());
    Rc::new(move || dropped(&app, &pc, &role, &era, mine))
}

/// Puts the caret where the next paste belongs.
fn focus(id: &str) {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(id))
        .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
    {
        let _ = el.focus();
    }
}

/// Acts on a link or SDP from anywhere — the box, the clipboard, the URL.
///
/// `complain` is for the paths where the user pressed something and deserves
/// an answer. The box itself fires on every keystroke, so half-typed text has
/// to fail in silence.
#[allow(clippy::too_many_arguments)]
fn accept(
    pc: &Pc,
    app: &app::Shared,
    ta: &HtmlTextAreaElement,
    role: &Rc<Cell<Role>>,
    era: &Era,
    text: &str,
    complain: bool,
) {
    let was = role.get();
    if was == Role::Done {
        return;
    }
    // Bail before the async hop on anything that is obviously not a link, so
    // typing in the box does not queue work on every character.
    if shape_of(text).is_none() {
        if complain {
            net::note("that does not look like a link — paste the whole thing");
        }
        return;
    }
    let (pc, app, ta, role, era, text) = (
        pc.clone(),
        app.clone(),
        ta.clone(),
        role.clone(),
        era.clone(),
        text.to_string(),
    );
    spawn_local(async move {
        let Some(sdp) = sdp_from_input(&text).await else {
            if complain {
                net::note("that link did not decode — copy it again, all of it");
            }
            return;
        };
        // Re-check: decoding took a turn of the event loop, and a second
        // paste may have got here first.
        if role.get() == Role::Done {
            return;
        }
        role.set(Role::Done);
        match was {
            Role::Host => finish_host(pc, role, era, &sdp).await,
            _ => join_flow(pc, app, ta, role, era, sdp).await,
        }
    });
}

/// Is there an offer in our own address bar? Cheap and synchronous, so the
/// decision to start a join can be made before the async decode.
fn url_has_offer() -> bool {
    web_sys::window()
        .and_then(|w| w.location().hash().ok())
        .is_some_and(|h| h.starts_with("#o="))
}

/// The offer carried in our own address bar. Goes through the same decode as
/// a paste, which is what makes a compressed link work in the address bar as
/// well as in the box.
async fn offer_from_url() -> Option<String> {
    let hash = web_sys::window()?.location().hash().ok()?;
    if !hash.starts_with("#o=") {
        return None;
    }
    sdp_from_input(&hash).await
}

pub fn wire(doc: &Document, app: app::Shared) -> Result<(), JsValue> {
    let ta: HtmlTextAreaElement = doc.get_element_by_id("sig").ok_or("no #sig")?.dyn_into()?;
    let go_btn = doc.get_element_by_id("go").ok_or("no #go")?;

    // Owns the connection for the life of the page; the App owns the channel.
    let pc: Pc = Rc::new(RefCell::new(None));
    let role = Rc::new(Cell::new(Role::Idle));
    let era: Era = Rc::new(Cell::new(0));

    // One button, one job at a time: start the game before there is a link,
    // re-copy the link after there is one.
    {
        let (pc, app, ta, role, era) = (
            pc.clone(),
            app.clone(),
            ta.clone(),
            role.clone(),
            era.clone(),
        );
        let cb = Closure::<dyn FnMut()>::new(move || {
            if role.get() != Role::Idle {
                copy(&ta);
                net::note(if role.get() == Role::Host {
                    "copied again — send it, their reply goes in the box below"
                } else {
                    "copied again — send it back to the host to finish connecting"
                });
                return;
            }
            let (pc, app, ta, role, era) = (
                pc.clone(),
                app.clone(),
                ta.clone(),
                role.clone(),
                era.clone(),
            );
            spawn_local(async move { host_flow(pc, app, ta, role, era).await });
        });
        go_btn.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref())?;
        cb.forget();
    }

    // A link that lands in either box is acted on immediately — there is never
    // anything else to do with it. `set_value` does not fire `input`, so our
    // own generated links cannot trigger this. `#sig` is where a joiner pastes
    // an offer; `#reply` is where the host pastes the answer.
    for id in ["sig", "reply"] {
        let src: HtmlTextAreaElement = doc.get_element_by_id(id).ok_or("no box")?.dyn_into()?;
        let (pc, app, role, ta, era, s) = (
            pc.clone(),
            app.clone(),
            role.clone(),
            ta.clone(),
            era.clone(),
            src.clone(),
        );
        let cb = Closure::<dyn FnMut()>::new(move || {
            accept(&pc, &app, &ta, &role, &era, &s.value(), false);
        });
        src.add_event_listener_with_callback("input", cb.as_ref().unchecked_ref())?;
        cb.forget();
    }

    // Connect: pasting already fires the flow, but a button is what people
    // look for after filling a box, and it is the way in for typed text.
    {
        let (pc, app, role, ta, era) = (
            pc.clone(),
            app.clone(),
            role.clone(),
            ta.clone(),
            era.clone(),
        );
        let reply: HtmlTextAreaElement = doc
            .get_element_by_id("reply")
            .ok_or("no #reply")?
            .dyn_into()?;
        let cb = Closure::<dyn FnMut()>::new(move || {
            accept(&pc, &app, &ta, &role, &era, &reply.value(), true);
        });
        doc.get_element_by_id("connect")
            .ok_or("no #connect")?
            .add_event_listener_with_callback("click", cb.as_ref().unchecked_ref())?;
        cb.forget();
    }

    // Join: the box already accepts a pasted link, but pasting on a phone is
    // a long-press and a menu. This asks the clipboard directly.
    {
        let (pc, app, role, era) = (pc.clone(), app.clone(), role.clone(), era.clone());
        let ta_j = ta.clone();
        let cb = Closure::<dyn FnMut()>::new(move || {
            if role.get() != Role::Idle {
                return;
            }
            ta_j.focus().ok();
            let (pc, app, ta, role, era) = (
                pc.clone(),
                app.clone(),
                ta_j.clone(),
                role.clone(),
                era.clone(),
            );
            spawn_local(async move {
                // ponytail: no clipboard-permission dance. Firefox refuses
                // readText outright, so a failure just means "type it in".
                let read = web_sys::window().map(|w| w.navigator().clipboard().read_text());
                let text = match read {
                    Some(p) => JsFuture::from(p).await.ok().and_then(|v| v.as_string()),
                    None => None,
                };
                match text.filter(|t| shape_of(t).is_some()) {
                    Some(t) => {
                        net::note("link read from clipboard — answering…");
                        accept(&pc, &app, &ta, &role, &era, &t, true);
                    }
                    None => net::note("paste the host's link into the box below"),
                }
            });
        });
        doc.get_element_by_id("join")
            .ok_or("no #join")?
            .add_event_listener_with_callback("click", cb.as_ref().unchecked_ref())?;
        cb.forget();
    }

    // A link that arrives while the tab is already open only changes the
    // fragment — the browser does not reload, so `main` never runs again and
    // the check below would never see it. Pasting a link into the address bar
    // of an open game is an ordinary thing to do.
    {
        let (pc, app, ta, role, era) = (
            pc.clone(),
            app.clone(),
            ta.clone(),
            role.clone(),
            era.clone(),
        );
        let cb = Closure::<dyn FnMut()>::new(move || {
            if role.get() == Role::Done || !url_has_offer() {
                return;
            }
            net::note("link in the address bar — answering…");
            role.set(Role::Done);
            let (pc, app, ta, role, era) = (
                pc.clone(),
                app.clone(),
                ta.clone(),
                role.clone(),
                era.clone(),
            );
            spawn_local(async move {
                match offer_from_url().await {
                    Some(offer) => join_flow(pc, app, ta, role, era, offer).await,
                    None => {
                        // Let them try again rather than sitting in Done.
                        role.set(Role::Idle);
                        net::note("that link did not decode — copy it again, all of it");
                    }
                }
            });
        });
        web_sys::window()
            .ok_or("no window")?
            .add_event_listener_with_callback("hashchange", cb.as_ref().unchecked_ref())?;
        cb.forget();
    }

    // Opened from a host's link: answer it without waiting to be asked.
    if url_has_offer() {
        net::note("opened from a link — answering…");
        role.set(Role::Done);
        let r = role.clone();
        spawn_local(async move {
            match offer_from_url().await {
                Some(offer) => join_flow(pc, app, ta, r, era, offer).await,
                None => {
                    r.set(Role::Idle);
                    net::note("that link did not decode — ask for it again");
                }
            }
        });
    }

    Ok(())
}

async fn host_flow(
    pc: Pc,
    app: app::Shared,
    ta: HtmlTextAreaElement,
    role: Rc<Cell<Role>>,
    era: Era,
) {
    let mine = era.get();
    net::note("gathering candidates…");
    let conn = match net::new_connection() {
        Ok(c) => c,
        Err(e) => return net::note(&format!("connection failed: {e:?}")),
    };
    // Claim the seat now, not when the channel opens: the peer's opening log
    // can arrive before `on_open` resolves, and `remote` decides what to do
    // with it by asking whether we are the host. `seat` keeps it if we have
    // already played in this game.
    {
        let mut a = app.borrow_mut();
        a.player = app::seat(a.player, true, &a.log);
    }
    net::trace_states(&conn, "host", on_drop(&app, &pc, &role, &era, mine));
    match net::make_offer(&conn).await {
        Ok((ch, sdp)) => {
            watch(ch, app, "host", pc.clone(), role.clone(), era.clone(), mine);
            let link = make_link('o', &sdp).await;
            ta.set_value(&link);
            copy(&ta);
            show_qr(&link);
            set_go("Copy the link again");
            display("join", "none");
            // The box above now holds our own link, so the reply needs a box
            // of its own — otherwise there is nowhere obvious to paste it.
            display("replybox", "grid");
            focus("reply");
            *pc.borrow_mut() = Some(conn);
            role.set(Role::Host);
            net::log(&format!(
                "[host] candidates: {} ({} chars)",
                net::candidate_summary(&sdp),
                link.len()
            ));
            net::note(
                "link copied — send it, or have them scan the QR. Their reply goes in the box below.",
            );
        }
        Err(e) => {
            role.set(Role::Idle);
            net::note(&format!(
                "could not make a link ({e:?}) — press Host to try again"
            ));
        }
    }
}

async fn join_flow(
    pc: Pc,
    app: app::Shared,
    ta: HtmlTextAreaElement,
    role: Rc<Cell<Role>>,
    era: Era,
    offer: String,
) {
    let mine = era.get();
    net::note("gathering candidates…");
    let conn = match net::new_connection() {
        Ok(c) => c,
        Err(e) => return net::note(&format!("connection failed: {e:?}")),
    };
    // Same reason as in `host_flow`: the host's log may beat our `on_open`.
    {
        let mut a = app.borrow_mut();
        a.player = app::seat(a.player, false, &a.log);
    }
    net::trace_states(&conn, "join", on_drop(&app, &pc, &role, &era, mine));
    net::log(&format!(
        "[join] their candidates: {}",
        net::candidate_summary(&offer)
    ));

    // Installed before the answer is created — the channel can arrive the
    // moment the connection completes.
    let incoming = net::on_data_channel(&conn);
    {
        let (app, pc, role, era) = (app.clone(), pc.clone(), role.clone(), era.clone());
        spawn_local(async move {
            if let Ok(Ok(ch)) = incoming.await.map(|v| v.dyn_into::<RtcDataChannel>()) {
                watch(ch, app, "join", pc, role, era, mine);
            }
        });
    }

    match net::accept_offer(&conn, &offer).await {
        Ok(answer) => {
            let link = make_link('a', &answer).await;
            ta.set_value(&link);
            copy(&ta);
            show_qr(&link);
            // The joiner has done everything they can; the label is the
            // instruction, because the button is the only thing they can press.
            set_go("Send this reply back to the host — tap to copy it again");
            display("join", "none");
            *pc.borrow_mut() = Some(conn);
            net::log(&format!(
                "[join] candidates: {}",
                net::candidate_summary(&answer)
            ));
            net::note("reply copied — send it back to the host and they will be connected to you");
        }
        Err(e) => {
            // Back to Idle, or a mistyped link would be the end of the session:
            // every later paste is swallowed by the Done guard.
            role.set(Role::Idle);
            net::note(&format!(
                "that link did not work ({e:?}) — ask for a fresh one"
            ));
        }
    }
}

async fn finish_host(pc: Pc, role: Rc<Cell<Role>>, era: Era, answer: &str) {
    let Some(conn) = pc.borrow().clone() else {
        role.set(Role::Idle);
        return net::note("no connection to answer — press Host to start again");
    };
    match net::accept_answer(&conn, answer).await {
        Ok(()) => net::note("reply accepted — connecting…"),
        Err(e) => {
            // The connection is spent once a bad answer has been applied to
            // it, so "paste it again" would be a lie — the old link is dead
            // with it. Back to a clean panel and a fresh link.
            net::log(&format!("bad reply: {e:?}"));
            reset_handshake(&pc, &role, &era);
            net::note("that reply did not work — press Host for a new link");
        }
    }
}

/// Stores the channel and reports when it opens. Both sides end up here — the
/// host from `create_data_channel`, the joiner from `ondatachannel`.
#[allow(clippy::too_many_arguments)]
fn watch(
    ch: RtcDataChannel,
    app: app::Shared,
    tag: &'static str,
    pc: Pc,
    role: Rc<Cell<Role>>,
    era: Era,
    mine: u32,
) {
    net::log(&format!("[{tag}] channel {:?}", ch.ready_state()));

    // Default binaryType is "blob", which would hand us a Blob needing an
    // async read. Arraybuffer is synchronous and is what we encode to.
    ch.set_binary_type(RtcDataChannelType::Arraybuffer);

    {
        let app = app.clone();
        let on_msg =
            Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
                let bytes = js_sys::Uint8Array::new(&e.data()).to_vec();
                app::remote(&app, &bytes);
            });
        ch.set_onmessage(Some(on_msg.as_ref().unchecked_ref()));
        on_msg.forget();
    }

    // A drop is not the end of the game: the log stays, the handshake comes
    // back, and reconnecting merges whatever each side missed.
    {
        let (app, pc, role, era) = (app.clone(), pc.clone(), role.clone(), era.clone());
        let on_close = Closure::<dyn FnMut()>::new(move || dropped(&app, &pc, &role, &era, mine));
        ch.set_onclose(Some(on_close.as_ref().unchecked_ref()));
        on_close.forget();
    }

    let opened = net::on_open(&ch);
    app.borrow_mut().chan = Some(ch);
    spawn_local(async move {
        match opened.await {
            Ok(_) => {
                display("handshake", "none");
                net::note(if tag == "host" {
                    "connected — you are red, they are blue, same board for both"
                } else {
                    "connected — you are blue, they are red, same board for both"
                });
                app::on_connect(&app, tag == "host");
            }
            Err(e) => net::note(&format!("[{tag}] channel failed: {e:?}")),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The decompression itself needs a browser, so what is checked here is
    /// the part that decides *what a paste is* — every shape a person might
    /// hand us has to come out as the same payload.
    #[test]
    fn input_accepts_every_shape_a_user_might_paste() {
        let sdp = "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\n";
        let packed = b"not really deflate, but opaque bytes either way";
        let b64 = b64::encode(packed);
        let want = Pasted::Packed(packed.to_vec());

        // The SDP itself is passed through untouched, terminator and all.
        assert_eq!(shape_of(sdp), Some(Pasted::Sdp(sdp.to_string())));
        assert_eq!(shape_of(&b64), Some(want.clone()));
        assert_eq!(shape_of(&format!("#a={b64}")), Some(want.clone()));
        assert_eq!(
            shape_of(&format!("  https://x.dev/mine/#o={b64}  ")),
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

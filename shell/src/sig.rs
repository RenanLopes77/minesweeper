//! The page's half of the handshake.
//!
//! The state machine, the flows, and the wire live in `p2p_link::Session`;
//! this module is only what they look like on this page — which box a link
//! lands in, which panels appear, what the status line says about the game.
//! The division is the point: everything here touches the DOM, nothing here
//! decides anything.

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Document, HtmlTextAreaElement};

use crate::app;
use p2p_link::{Hooks, LinkKind, Role, Session, net, session};

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

/// What the session's events mean on this page. The game state rides in
/// `app`; the link box is where both generated links land.
fn hooks(app: app::Shared, ta: HtmlTextAreaElement) -> Hooks {
    Hooks {
        // Claim the seat as soon as the role is known, not when the channel
        // opens: the peer's opening log can arrive before `on_open`, and
        // `remote` decides what to do with it by asking whether we are the
        // host. `seat` keeps our seat if we have already played in this game.
        on_role: {
            let app = app.clone();
            Box::new(move |is_host| {
                let mut a = app.borrow_mut();
                a.player = app::seat(a.player, is_host, &a.log);
            })
        },
        on_link: Box::new(move |kind, link| {
            ta.set_value(link);
            copy(&ta);
            show_qr(link);
            display("join", "none");
            match kind {
                LinkKind::Offer => {
                    set_go("Copy the link again");
                    // The box above now holds our own link, so the reply
                    // needs a box of its own — otherwise there is nowhere
                    // obvious to paste it.
                    display("replybox", "grid");
                    focus("reply");
                    net::note(
                        "link copied — send it, or have them scan the QR. Their reply goes in the box below.",
                    );
                }
                LinkKind::Answer => {
                    // The joiner has done everything they can; the label is
                    // the instruction, because the button is the only thing
                    // they can press.
                    set_go("Send this reply back to the host — tap to copy it again");
                    net::note(
                        "reply copied — send it back to the host and they will be connected to you",
                    );
                }
            }
        }),
        on_channel: {
            let app = app.clone();
            Box::new(move |ch, _| app.borrow_mut().chan = Some(ch))
        },
        on_open: {
            let app = app.clone();
            Box::new(move |is_host| {
                display("handshake", "none");
                net::note(if is_host {
                    "connected — you are red, they are blue, same board for both"
                } else {
                    "connected — you are blue, they are red, same board for both"
                });
                app::on_connect(&app, is_host);
            })
        },
        on_message: {
            let app = app.clone();
            Box::new(move |bytes| app::remote(&app, &bytes))
        },
        // A drop is not the end of the game: the log stays, the handshake
        // comes back, and reconnecting merges whatever each side missed.
        on_drop: Box::new(move || {
            app.borrow_mut().chan = None;
            net::note("connection lost — your board is kept. Host again, or paste a new link.");
        }),
        on_reset: Box::new(|| {
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
        }),
    }
}

pub fn wire(doc: &Document, app: app::Shared) -> Result<(), JsValue> {
    let ta: HtmlTextAreaElement = doc.get_element_by_id("sig").ok_or("no #sig")?.dyn_into()?;
    let go_btn = doc.get_element_by_id("go").ok_or("no #go")?;

    let sess = Session::new(hooks(app, ta.clone()));

    // One button, one job at a time: start the game before there is a link,
    // re-copy the link after there is one.
    {
        let (sess, ta) = (sess.clone(), ta.clone());
        let cb = Closure::<dyn FnMut()>::new(move || {
            if sess.role() != Role::Idle {
                copy(&ta);
                net::note(if sess.role() == Role::Host {
                    "copied again — send it, their reply goes in the box below"
                } else {
                    "copied again — send it back to the host to finish connecting"
                });
                return;
            }
            sess.host();
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
        let (sess, s) = (sess.clone(), src.clone());
        let cb = Closure::<dyn FnMut()>::new(move || {
            sess.accept(&s.value(), false);
        });
        src.add_event_listener_with_callback("input", cb.as_ref().unchecked_ref())?;
        cb.forget();
    }

    // Connect: pasting already fires the flow, but a button is what people
    // look for after filling a box, and it is the way in for typed text.
    {
        let sess = sess.clone();
        let reply: HtmlTextAreaElement = doc
            .get_element_by_id("reply")
            .ok_or("no #reply")?
            .dyn_into()?;
        let cb = Closure::<dyn FnMut()>::new(move || {
            sess.accept(&reply.value(), true);
        });
        doc.get_element_by_id("connect")
            .ok_or("no #connect")?
            .add_event_listener_with_callback("click", cb.as_ref().unchecked_ref())?;
        cb.forget();
    }

    // Join: the box already accepts a pasted link, but pasting on a phone is
    // a long-press and a menu. This asks the clipboard directly.
    {
        let sess = sess.clone();
        let ta_j = ta.clone();
        let cb = Closure::<dyn FnMut()>::new(move || {
            if sess.role() != Role::Idle {
                return;
            }
            ta_j.focus().ok();
            let sess = sess.clone();
            wasm_bindgen_futures::spawn_local(async move {
                // ponytail: no clipboard-permission dance. Firefox refuses
                // readText outright, so a failure just means "type it in".
                let read = web_sys::window().map(|w| w.navigator().clipboard().read_text());
                let text = match read {
                    Some(p) => JsFuture::from(p).await.ok().and_then(|v| v.as_string()),
                    None => None,
                };
                match text.filter(|t| Session::looks_like_link(t)) {
                    Some(t) => {
                        net::note("link read from clipboard — answering…");
                        sess.accept(&t, true);
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
    // the load-time check below would never see it. Pasting a link into the
    // address bar of an open game is an ordinary thing to do.
    {
        let sess = sess.clone();
        let cb = Closure::<dyn FnMut()>::new(move || {
            if sess.role() == Role::Done || !session::url_has_offer() {
                return;
            }
            net::note("link in the address bar — answering…");
            sess.join_url_offer();
        });
        web_sys::window()
            .ok_or("no window")?
            .add_event_listener_with_callback("hashchange", cb.as_ref().unchecked_ref())?;
        cb.forget();
    }

    // Opened from a host's link: answer it without waiting to be asked.
    if session::url_has_offer() {
        net::note("opened from a link — answering…");
        sess.join_url_offer();
    }

    Ok(())
}

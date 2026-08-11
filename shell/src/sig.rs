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
use wasm_bindgen_futures::spawn_local;
use web_sys::{
    Document, HtmlTextAreaElement, RtcDataChannel, RtcDataChannelType, RtcPeerConnection,
};

use crate::{app, b64, net};

/// Who we are in the handshake. Decides what Accept does with the box.
#[derive(Clone, Copy, PartialEq)]
enum Role {
    /// Nothing started. Pasted text is an offer to answer.
    Idle,
    /// We made the offer. Pasted text is the answer that completes it.
    Host,
}

type Pc = Rc<RefCell<Option<RtcPeerConnection>>>;

/// `#o=` for an offer, `#a=` for an answer.
fn make_link(kind: char, sdp: &str) -> String {
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
    format!("{base}#{kind}={}", b64::encode(sdp.as_bytes()))
}

/// Accepts whatever the user pasted: a whole link, a bare `#a=…` fragment,
/// raw base64, or the SDP itself. People paste all four.
fn sdp_from_input(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    // Already an SDP: hand it back untouched. Trimming here is what ate the
    // final line's terminator once already — net::normalize_sdp owns cleanup.
    if t.starts_with("v=") {
        return Some(s.to_string());
    }
    let payload = match t.rfind(['#', '=']) {
        Some(i) => &t[i + 1..],
        None => t,
    };
    if payload.is_empty() {
        return None;
    }
    String::from_utf8(b64::decode(payload)?).ok()
}

/// The offer carried in our own address bar, if we were opened from a link.
fn offer_from_url() -> Option<String> {
    let hash = web_sys::window()?.location().hash().ok()?;
    let rest = hash.strip_prefix("#o=")?;
    String::from_utf8(b64::decode(rest)?).ok()
}

pub fn wire(doc: &Document, app: app::Shared) -> Result<(), JsValue> {
    let ta: HtmlTextAreaElement = doc.get_element_by_id("sig").ok_or("no #sig")?.dyn_into()?;
    let host_btn = doc.get_element_by_id("host").ok_or("no #host")?;
    let accept_btn = doc.get_element_by_id("accept").ok_or("no #accept")?;

    // Owns the connection for the life of the page; the App owns the channel.
    let pc: Pc = Rc::new(RefCell::new(None));
    let role = Rc::new(Cell::new(Role::Idle));

    {
        let (pc, app, ta, role) = (pc.clone(), app.clone(), ta.clone(), role.clone());
        let cb = Closure::<dyn FnMut()>::new(move || {
            let (pc, app, ta, role) = (pc.clone(), app.clone(), ta.clone(), role.clone());
            spawn_local(async move { host_flow(pc, app, ta, role).await });
        });
        host_btn.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref())?;
        cb.forget();
    }

    {
        let (pc, app, ta, role) = (pc.clone(), app.clone(), ta.clone(), role.clone());
        let cb = Closure::<dyn FnMut()>::new(move || {
            let (pc, app, ta, role) = (pc.clone(), app.clone(), ta.clone(), role.clone());
            spawn_local(async move {
                let Some(sdp) = sdp_from_input(&ta.value()) else {
                    return net::note("that does not look like a link or an SDP");
                };
                match role.get() {
                    Role::Host => finish_host(pc, &sdp).await,
                    Role::Idle => join_flow(pc, app, ta, sdp).await,
                }
            });
        });
        accept_btn.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref())?;
        cb.forget();
    }

    {
        // select() alone is enough on a desktop; on a phone there is no
        // convenient Ctrl+C, so ask the clipboard directly.
        let ta = ta.clone();
        let copy_btn = doc.get_element_by_id("copy").ok_or("no #copy")?;
        let cb = Closure::<dyn FnMut()>::new(move || {
            ta.select();
            let text = ta.value();
            if let Some(win) = web_sys::window() {
                let _ = win.navigator().clipboard().write_text(&text);
                net::note("copied — send it over");
            }
        });
        copy_btn.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref())?;
        cb.forget();
    }

    // Opened from a host's link: answer it without waiting to be asked.
    if let Some(offer) = offer_from_url() {
        net::note("opened from a link — answering…");
        spawn_local(async move { join_flow(pc, app, ta, offer).await });
    }

    Ok(())
}

async fn host_flow(pc: Pc, app: app::Shared, ta: HtmlTextAreaElement, role: Rc<Cell<Role>>) {
    net::note("gathering candidates…");
    let conn = match net::new_connection() {
        Ok(c) => c,
        Err(e) => return net::note(&format!("connection failed: {e:?}")),
    };
    net::trace_states(&conn, "host");
    match net::make_offer(&conn).await {
        Ok((ch, sdp)) => {
            watch(ch, app, "host");
            let link = make_link('o', &sdp);
            ta.set_value(&link);
            ta.select();
            *pc.borrow_mut() = Some(conn);
            role.set(Role::Host);
            net::log(&format!(
                "[host] candidates: {}",
                net::candidate_summary(&sdp)
            ));
            net::note(&format!(
                "link ready ({} chars) — send it, then paste their reply here",
                link.len()
            ));
        }
        Err(e) => net::note(&format!("offer failed: {e:?}")),
    }
}

async fn join_flow(pc: Pc, app: app::Shared, ta: HtmlTextAreaElement, offer: String) {
    net::note("gathering candidates…");
    let conn = match net::new_connection() {
        Ok(c) => c,
        Err(e) => return net::note(&format!("connection failed: {e:?}")),
    };
    net::trace_states(&conn, "join");
    net::log(&format!(
        "[join] their candidates: {}",
        net::candidate_summary(&offer)
    ));

    // Installed before the answer is created — the channel can arrive the
    // moment the connection completes.
    let incoming = net::on_data_channel(&conn);
    {
        let app = app.clone();
        spawn_local(async move {
            if let Ok(Ok(ch)) = incoming.await.map(|v| v.dyn_into::<RtcDataChannel>()) {
                watch(ch, app, "join");
            }
        });
    }

    match net::accept_offer(&conn, &offer).await {
        Ok(answer) => {
            let link = make_link('a', &answer);
            ta.set_value(&link);
            ta.select();
            *pc.borrow_mut() = Some(conn);
            net::log(&format!(
                "[join] candidates: {}",
                net::candidate_summary(&answer)
            ));
            net::note("reply ready — send this link back to the host");
        }
        Err(e) => net::note(&format!("bad offer: {e:?}")),
    }
}

async fn finish_host(pc: Pc, answer: &str) {
    let Some(conn) = pc.borrow().clone() else {
        return net::note("no connection to answer");
    };
    match net::accept_answer(&conn, answer).await {
        Ok(()) => net::note("reply accepted — connecting…"),
        Err(e) => net::note(&format!("bad reply: {e:?}")),
    }
}

/// Stores the channel and reports when it opens. Both sides end up here — the
/// host from `create_data_channel`, the joiner from `ondatachannel`.
fn watch(ch: RtcDataChannel, app: app::Shared, tag: &'static str) {
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

    let opened = net::on_open(&ch);
    app.borrow_mut().chan = Some(ch);
    spawn_local(async move {
        match opened.await {
            Ok(_) => {
                net::note(&format!("CONNECTED ({tag})"));
                app::on_connect(&app, tag == "host");
            }
            Err(e) => net::note(&format!("[{tag}] channel failed: {e:?}")),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_accepts_every_shape_a_user_might_paste() {
        let sdp = "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\n";
        let b64 = b64::encode(sdp.as_bytes());

        assert_eq!(sdp_from_input(sdp).as_deref(), Some(sdp));
        assert_eq!(sdp_from_input(&b64).as_deref(), Some(sdp));
        assert_eq!(sdp_from_input(&format!("#a={b64}")).as_deref(), Some(sdp));
        assert_eq!(
            sdp_from_input(&format!("  https://x.dev/mine/#o={b64}  ")).as_deref(),
            Some(sdp)
        );
    }

    #[test]
    fn input_rejects_junk() {
        assert!(sdp_from_input("").is_none());
        assert!(sdp_from_input("hello world").is_none());
    }
}

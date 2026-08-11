//! Signalling by clipboard.
//!
//! Two buttons and a textarea. The host presses Host, copies the blob, and
//! sends it over by whatever means — chat, email, a photo of the screen. The
//! joiner pastes it, presses Accept, and sends the reply back the same way.
//! One round trip, entirely by hand. That hand is the signalling server.
//!
//! The SDP is not compressed or encoded here. A clipboard has no size limit,
//! so shrinking it buys nothing until the blob has to fit in a QR code.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;
use web_sys::{
    Document, HtmlTextAreaElement, RtcDataChannel, RtcDataChannelType, RtcPeerConnection,
};

use crate::{app, net};

/// Who we are in the handshake. Decides what the Accept button does with
/// whatever is in the textarea.
#[derive(Clone, Copy, PartialEq)]
enum Role {
    /// Nothing started. Pasted text is an offer to answer.
    Idle,
    /// We made the offer. Pasted text is the answer that completes it.
    Host,
}

pub fn wire(doc: &Document, app: app::Shared) -> Result<(), JsValue> {
    let ta: HtmlTextAreaElement = doc.get_element_by_id("sig").ok_or("no #sig")?.dyn_into()?;
    let host_btn = doc.get_element_by_id("host").ok_or("no #host")?;
    let accept_btn = doc.get_element_by_id("accept").ok_or("no #accept")?;

    // The connection needs an owner that outlives this function; the App now
    // holds the channel, and this holds the peer connection.
    let pc: Rc<RefCell<Option<RtcPeerConnection>>> = Rc::new(RefCell::new(None));
    let role = Rc::new(Cell::new(Role::Idle));

    {
        let (pc, app, ta, role) = (pc.clone(), app.clone(), ta.clone(), role.clone());
        let cb = Closure::<dyn FnMut()>::new(move || {
            let (pc, app, ta, role) = (pc.clone(), app.clone(), ta.clone(), role.clone());
            spawn_local(async move {
                net::note("gathering candidates…");
                let conn = match net::new_connection() {
                    Ok(c) => c,
                    Err(e) => return net::note(&format!("connection failed: {e:?}")),
                };
                net::trace_states(&conn, "host");
                match net::make_offer(&conn).await {
                    Ok((ch, sdp)) => {
                        watch(ch, app, "host");
                        ta.set_value(&sdp);
                        ta.select();
                        *pc.borrow_mut() = Some(conn);
                        role.set(Role::Host);
                        net::log(&format!(
                            "[host] candidates: {}",
                            net::candidate_summary(&sdp)
                        ));
                        net::note(&format!(
                            "offer ready ({} bytes) — send it over, paste the reply here",
                            sdp.len()
                        ));
                    }
                    Err(e) => net::note(&format!("offer failed: {e:?}")),
                }
            });
        });
        host_btn.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref())?;
        cb.forget();
    }

    {
        let (pc, app, ta, role) = (pc.clone(), app.clone(), ta.clone(), role.clone());
        let cb = Closure::<dyn FnMut()>::new(move || {
            let (pc, app, ta, role) = (pc.clone(), app.clone(), ta.clone(), role.clone());
            spawn_local(async move {
                // Not trimmed here — net::normalize_sdp owns that, and doing it
                // in two places is how the terminator got eaten the first time.
                let pasted = ta.value();
                if pasted.trim().is_empty() {
                    return net::note("nothing pasted");
                }
                match role.get() {
                    // We hosted; this is the answer coming back.
                    Role::Host => {
                        let conn = match pc.borrow().clone() {
                            Some(c) => c,
                            None => return net::note("no connection to answer"),
                        };
                        match net::accept_answer(&conn, &pasted).await {
                            Ok(()) => net::note("answer accepted — connecting…"),
                            Err(e) => net::note(&format!("bad answer: {e:?}")),
                        }
                    }
                    // We are joining; this is their offer.
                    Role::Idle => {
                        net::note("gathering candidates…");
                        let conn = match net::new_connection() {
                            Ok(c) => c,
                            Err(e) => return net::note(&format!("connection failed: {e:?}")),
                        };
                        net::trace_states(&conn, "join");
                        net::log(&format!(
                            "[join] their candidates: {}",
                            net::candidate_summary(&pasted)
                        ));
                        // Installed before the answer is created — the channel
                        // can arrive the moment the connection completes.
                        let incoming = net::on_data_channel(&conn);
                        {
                            let app = app.clone();
                            spawn_local(async move {
                                net::log("[join] waiting for ondatachannel");
                                if let Ok(Ok(ch)) =
                                    incoming.await.map(|v| v.dyn_into::<RtcDataChannel>())
                                {
                                    watch(ch, app, "join");
                                }
                            });
                        }
                        match net::accept_offer(&conn, &pasted).await {
                            Ok(answer) => {
                                ta.set_value(&answer);
                                ta.select();
                                *pc.borrow_mut() = Some(conn);
                                net::log(&format!(
                                    "[join] candidates: {}",
                                    net::candidate_summary(&answer)
                                ));
                                net::note(&format!(
                                    "answer ready ({} bytes) — send it back",
                                    answer.len()
                                ));
                            }
                            Err(e) => net::note(&format!("bad offer: {e:?}")),
                        }
                    }
                }
            });
        });
        accept_btn.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref())?;
        cb.forget();
    }

    Ok(())
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

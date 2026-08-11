//! WebRTC signalling: enough to turn two browsers into one DataChannel.
//!
//! Deliberately *non-trickle*. The usual WebRTC flow streams ICE candidates
//! to the peer as they are discovered, which needs a live signalling channel.
//! We have no such channel — the whole point is that a human carries the
//! handshake across by QR code or clipboard. So we wait for ICE gathering to
//! finish and hand back one self-contained SDP blob.
//!
//! Cost: the handshake takes a second or two longer. Benefit: no server.

use js_sys::{Promise, Reflect};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    RtcConfiguration, RtcDataChannel, RtcDataChannelState, RtcIceGatheringState, RtcIceServer,
    RtcPeerConnection, RtcSdpType, RtcSessionDescriptionInit,
};

/// A public STUN server. This is the one piece of infrastructure the project
/// cannot avoid: peers behind NAT need it to learn their own public address.
/// It sees an IP and nothing else — no game data passes through it.
const STUN: &str = "stun:stun.l.google.com:19302";

/// Unused until the real two-device flow lands in ch12 — the loopback
/// self-test below deliberately builds STUN-free connections instead.
#[allow(dead_code)]
pub fn new_connection() -> Result<RtcPeerConnection, JsValue> {
    let ice = js_sys::Array::new();
    let server = RtcIceServer::new();
    server.set_urls(&JsValue::from_str(STUN));
    ice.push(&server);

    let cfg = RtcConfiguration::new();
    cfg.set_ice_servers(&ice);
    RtcPeerConnection::new_with_configuration(&cfg)
}

/// Resolves once ICE gathering is complete, so `local_description()` contains
/// every candidate we will ever have. Without this the SDP you copy out is
/// missing its candidates and the peer can never reach you.
async fn wait_for_ice(pc: &RtcPeerConnection) -> Result<(), JsValue> {
    if pc.ice_gathering_state() == RtcIceGatheringState::Complete {
        return Ok(());
    }
    let pc2 = pc.clone();
    let promise = Promise::new(&mut |resolve, _reject| {
        let pc3 = pc2.clone();
        let r = resolve.clone();
        let cb = Closure::<dyn FnMut()>::new(move || {
            if pc3.ice_gathering_state() == RtcIceGatheringState::Complete {
                let _ = r.call0(&JsValue::NULL);
            }
        });
        pc2.set_onicegatheringstatechange(Some(cb.as_ref().unchecked_ref()));
        cb.forget();
        // Re-check *after* installing the handler. Gathering can finish in the
        // window between the early return above and this line, and that event
        // is gone for good — the await would never resolve.
        if pc2.ice_gathering_state() == RtcIceGatheringState::Complete {
            let _ = resolve.call0(&JsValue::NULL);
        }
    });
    JsFuture::from(promise).await?;
    Ok(())
}

fn sdp_of(desc: &JsValue) -> Result<String, JsValue> {
    Reflect::get(desc, &JsValue::from_str("sdp"))?
        .as_string()
        .ok_or_else(|| JsValue::from_str("session description has no sdp"))
}

/// Caller side. Creates the DataChannel, then the offer — in that order,
/// because the channel has to exist before the offer is generated or the SDP
/// won't negotiate one.
pub async fn make_offer(pc: &RtcPeerConnection) -> Result<(RtcDataChannel, String), JsValue> {
    let chan = pc.create_data_channel("game");
    note("1a create_offer");
    let offer = JsFuture::from(pc.create_offer()).await?;

    note("1b set_local");
    let desc = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
    desc.set_sdp(&sdp_of(&offer)?);
    JsFuture::from(pc.set_local_description(&desc)).await?;

    note("1c gather");
    wait_for_ice(pc).await?;
    let sdp = pc
        .local_description()
        .ok_or_else(|| JsValue::from_str("no local description after gathering"))?
        .sdp();
    Ok((chan, sdp))
}

/// Callee side. Takes the offer SDP, returns the answer SDP to send back.
/// The DataChannel arrives asynchronously via `ondatachannel`, not from here.
pub async fn accept_offer(pc: &RtcPeerConnection, offer_sdp: &str) -> Result<String, JsValue> {
    let offer = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
    offer.set_sdp(offer_sdp);
    JsFuture::from(pc.set_remote_description(&offer)).await?;

    let answer = JsFuture::from(pc.create_answer()).await?;
    let desc = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
    desc.set_sdp(&sdp_of(&answer)?);
    JsFuture::from(pc.set_local_description(&desc)).await?;

    wait_for_ice(pc).await?;
    Ok(pc
        .local_description()
        .ok_or_else(|| JsValue::from_str("no local description after gathering"))?
        .sdp())
}

/// Caller side, final step: feed the answer back in and the channel opens.
pub async fn accept_answer(pc: &RtcPeerConnection, answer_sdp: &str) -> Result<(), JsValue> {
    let answer = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
    answer.set_sdp(answer_sdp);
    JsFuture::from(pc.set_remote_description(&answer)).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Event-to-future adapters.
//
// These are deliberately NOT `async fn`. An async fn's body does not run until
// it is awaited, so an `async fn wait_for_x` would install its event handler
// too late and miss an event that already fired. Returning a JsFuture from a
// plain fn installs the handler immediately, at call time, and lets you await
// it later — which is exactly the ordering WebRTC needs.
// ---------------------------------------------------------------------------

fn on_data_channel(pc: &RtcPeerConnection) -> JsFuture {
    let pc = pc.clone();
    JsFuture::from(Promise::new(&mut |resolve, _reject| {
        let cb = Closure::<dyn FnMut(web_sys::RtcDataChannelEvent)>::new(
            move |e: web_sys::RtcDataChannelEvent| {
                let _ = resolve.call1(&JsValue::NULL, &e.channel());
            },
        );
        pc.set_ondatachannel(Some(cb.as_ref().unchecked_ref()));
        cb.forget();
    }))
}

/// Note the state checks either side of installing the handler — the same
/// race as `wait_for_ice`. By the time the answer is applied, the caller's
/// channel is usually *already* open, so `onopen` fired before anyone was
/// listening and awaiting it would hang forever.
fn on_open(ch: &RtcDataChannel) -> JsFuture {
    let ch2 = ch.clone();
    JsFuture::from(Promise::new(&mut |resolve, _reject| {
        if ch2.ready_state() == RtcDataChannelState::Open {
            let _ = resolve.call0(&JsValue::NULL);
            return;
        }
        let r = resolve.clone();
        let cb = Closure::<dyn FnMut()>::new(move || {
            let _ = r.call0(&JsValue::NULL);
        });
        ch2.set_onopen(Some(cb.as_ref().unchecked_ref()));
        cb.forget();
        if ch2.ready_state() == RtcDataChannelState::Open {
            let _ = resolve.call0(&JsValue::NULL);
        }
    }))
}

fn on_message(ch: &RtcDataChannel) -> JsFuture {
    let ch2 = ch.clone();
    JsFuture::from(Promise::new(&mut |resolve, _reject| {
        let cb =
            Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
                let _ = resolve.call1(&JsValue::NULL, &e.data());
            });
        ch2.set_onmessage(Some(cb.as_ref().unchecked_ref()));
        cb.forget();
    }))
}

/// Writes progress into #status. When a WebRTC handshake stalls it stalls
/// silently, so knowing which await never returned is most of the diagnosis.
fn note(stage: &str) {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("status"))
    {
        el.set_text_content(Some(stage));
    }
}

/// Runs the whole handshake between two connections in this one tab and
/// pushes a message across. No network, no second device, no signalling —
/// but it exercises every line above, which is where the bugs are.
pub async fn loopback_selftest() -> Result<String, JsValue> {
    // No STUN here. Both peers are in this tab, so host candidates reach each
    // other, and gathering finishes immediately instead of waiting on a
    // round-trip to Google. A self-test that needs the internet to pass is a
    // self-test that fails on a train.
    let caller = RtcPeerConnection::new()?;
    let callee = RtcPeerConnection::new()?;

    // Installed before the handshake starts: the channel event can fire the
    // instant the answer is set, and a handler added afterwards misses it.
    let incoming = on_data_channel(&callee);

    note("1 offer");
    let (chan, offer) = make_offer(&caller).await?;
    note("2 answer");
    let answer = accept_offer(&callee, &offer).await?;
    note("3 accept");
    accept_answer(&caller, &answer).await?;

    note("4 channel");
    let far: RtcDataChannel = incoming.await?.dyn_into()?;
    let arrived = on_message(&far);
    note("5 open");
    on_open(&chan).await?;

    note("6 send");
    chan.send_with_str("ping")?;
    let got = arrived
        .await?
        .as_string()
        .ok_or_else(|| JsValue::from_str("message was not a string"))?;

    if got == "ping" {
        Ok(format!("PASS — channel open, {} bytes of SDP", offer.len()))
    } else {
        Err(JsValue::from_str(&format!("FAIL — received {got:?}")))
    }
}

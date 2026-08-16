//! Serverless WebRTC for two peers: the handshake travels as text a human
//! can carry — a link, a QR code, a paste — and comes back as an open
//! DataChannel. No signalling server, no accounts; one STUN lookup is the
//! only infrastructure touched.
//!
//! Extracted from a P2P Minesweeper. The crate is headless: it never touches
//! the page beyond `window` itself. Progress and transcript lines go to
//! whatever sinks the application installs via [`net::on_note`] and
//! [`net::on_log`], and everything UI-shaped stays with the caller.

pub mod b64;
pub mod net;
pub mod sdp;
pub mod zip;

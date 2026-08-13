//! Field-level SDP compression: send the facts, rebuild the file.
//!
//! An SDP for a data-channel-only connection is a page of boilerplate wrapped
//! around five facts the peer cannot guess: the ICE username and password,
//! the DTLS certificate fingerprint, the DTLS role, and the candidate
//! addresses. Everything else — the `v=`/`o=`/`m=` scaffolding, the sctp
//! port, the attribute names — is identical in every SDP a browser emits, so
//! both sides can carry it as a template instead of on the wire.
//!
//! Deflate got the handshake from 994 bytes to 568 by squeezing the
//! repetition; this gets it to ~130 by not sending the repetition at all.
//! The QR code is the reason to care: modules scale with payload, and the
//! difference is a code a phone reads at arm's length.
//!
//! `compact` is honest about its limits: an SDP it does not fully recognise —
//! a fingerprint that is not sha-256, a candidate shape we cannot rebuild —
//! returns `None`, and the caller falls back to deflating the whole text.
//! `expand` is a trust boundary like `wire.rs`: the bytes come from a paste,
//! so every read is checked and anything malformed is `None`, never a panic.
//!
//! Layout, after the magic byte:
//!
//! ```text
//! magic(1) setup(1) ufrag_len(1) ufrag pwd_len(1) pwd fp(32)
//! count(1) then per candidate: type(1) addr(4|16) port(2, LE)
//! ```
//!
//! Candidate types: 0 host over mDNS (a UUID, 16 bytes), 1 host IPv4,
//! 2 host IPv6, 3 srflx IPv4, 4 srflx IPv6. TCP candidates are dropped —
//! WebRTC data channels run over UDP in practice, and a lossy compact that
//! still connects beats a faithful one that does not fit in a QR code.

use std::fmt::Write as _;
use std::net::{Ipv4Addr, Ipv6Addr};

/// First byte of the compact form, so a paste can be told apart from the
/// deflated payloads older peers still send. Collisions are settled by
/// structure, not by this byte alone: `expand` demands every length line up
/// exactly, which no real deflate stream of this size survives.
const MAGIC: u8 = 0x01;

/// The DTLS role line. The offer is always `actpass` (RFC 5763 requires it),
/// but the answer picks, so the byte rides along rather than being assumed.
const SETUPS: [&str; 3] = ["actpass", "active", "passive"];

enum Addr {
    Mdns([u8; 16]),
    V4(Ipv4Addr),
    V6(Ipv6Addr),
}

struct Candidate {
    srflx: bool,
    addr: Addr,
    port: u16,
}

/// `1b932746-add2-4fb5-bdc5-99783097cabc.local` -> 16 bytes. Browsers hide
/// LAN addresses behind exactly this shape of mDNS name, which is why it gets
/// its own compact form instead of being shipped as 42 characters of text.
fn uuid_of(name: &str) -> Option<[u8; 16]> {
    let hex: Vec<u8> = name
        .strip_suffix(".local")?
        .bytes()
        .filter(|&b| b != b'-')
        .collect();
    if hex.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, pair) in hex.chunks(2).enumerate() {
        let s = std::str::from_utf8(pair).ok()?;
        out[i] = u8::from_str_radix(s, 16).ok()?;
    }
    Some(out)
}

fn uuid_text(b: &[u8; 16]) -> String {
    let mut s = String::with_capacity(42);
    for (i, byte) in b.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            s.push('-');
        }
        let _ = write!(s, "{byte:02x}");
    }
    s.push_str(".local");
    s
}

/// `a=candidate:2563029213 1 udp 2113937151 <addr> 45776 typ host ...`
/// Only what can be rebuilt: component 1, UDP, typ host or srflx. Anything
/// else returns `None` and the line is skipped.
fn candidate_of(line: &str) -> Option<Candidate> {
    let mut f = line.strip_prefix("a=candidate:")?.split_whitespace();
    let _foundation = f.next()?;
    if f.next()? != "1" {
        return None; // RTCP components died with rtcp-mux; only 1 exists
    }
    if !f.next()?.eq_ignore_ascii_case("udp") {
        return None;
    }
    let _priority = f.next()?;
    let addr_text = f.next()?;
    let port: u16 = f.next()?.parse().ok()?;
    if f.next()? != "typ" {
        return None;
    }
    let srflx = match f.next()? {
        "host" => false,
        "srflx" => true,
        _ => return None, // relay needs a TURN server we do not run
    };
    let addr = if let Some(uuid) = uuid_of(addr_text) {
        Addr::Mdns(uuid)
    } else if let Ok(v4) = addr_text.parse() {
        Addr::V4(v4)
    } else if let Ok(v6) = addr_text.parse() {
        Addr::V6(v6)
    } else {
        return None;
    };
    Some(Candidate { srflx, addr, port })
}

/// `3A:2B:...` (32 hex pairs) -> 32 bytes.
fn fingerprint_of(text: &str) -> Option<[u8; 32]> {
    let mut out = [0u8; 32];
    let mut pairs = text.split(':');
    for slot in &mut out {
        *slot = u8::from_str_radix(pairs.next()?, 16).ok()?;
    }
    pairs.next().is_none().then_some(out)
}

/// The value of `a=<name>:` in `sdp`, if the line is present.
fn attr<'a>(sdp: &'a str, name: &str) -> Option<&'a str> {
    sdp.lines()
        .map(|l| l.trim_end_matches('\r'))
        .find_map(|l| l.strip_prefix("a=")?.strip_prefix(name)?.strip_prefix(':'))
}

/// ICE ufrag/pwd characters, per RFC 8839. Checked on the way out *and* on
/// the way in: a byte outside this set could smuggle a newline — a whole
/// forged attribute — into the SDP `expand` rebuilds.
fn ice_chars(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
}

/// The compact form of a browser's SDP, or `None` for any SDP this module
/// does not fully understand — the caller deflates the whole text instead,
/// so an unexpected browser costs bytes, never the connection.
pub fn compact(sdp: &str) -> Option<Vec<u8>> {
    let setup = SETUPS.iter().position(|s| attr(sdp, "setup") == Some(*s))?;
    let ufrag = attr(sdp, "ice-ufrag")?;
    let pwd = attr(sdp, "ice-pwd")?;
    if !ice_chars(ufrag) || !ice_chars(pwd) || ufrag.len() > 255 || pwd.len() > 255 {
        return None;
    }
    let fp = fingerprint_of(attr(sdp, "fingerprint")?.strip_prefix("sha-256 ")?)?;

    let candidates: Vec<Candidate> = sdp
        .lines()
        .filter_map(|l| candidate_of(l.trim_end_matches('\r')))
        .collect();
    // No rebuildable candidate means the expanded SDP could never connect;
    // better to ship the original in full than a tidy dead end.
    if candidates.is_empty() || candidates.len() > 255 {
        return None;
    }

    let mut out = vec![MAGIC, setup as u8, ufrag.len() as u8];
    out.extend_from_slice(ufrag.as_bytes());
    out.push(pwd.len() as u8);
    out.extend_from_slice(pwd.as_bytes());
    out.extend_from_slice(&fp);
    out.push(candidates.len() as u8);
    for c in &candidates {
        let (tag, bytes): (u8, &[u8]) = match &c.addr {
            Addr::Mdns(u) => (0, u),
            Addr::V4(v4) => (if c.srflx { 3 } else { 1 }, &v4.octets()[..]),
            Addr::V6(v6) => (if c.srflx { 4 } else { 2 }, &v6.octets()[..]),
        };
        out.push(tag);
        out.extend_from_slice(bytes);
        out.extend_from_slice(&c.port.to_le_bytes());
    }
    Some(out)
}

/// Reads `n` bytes off the front of the input, or `None` if they are not
/// there. The whole decode is these two lines applied until the input is
/// exactly used up.
fn take<'a>(bytes: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
    let (head, rest) = bytes.split_at_checked(n)?;
    *bytes = rest;
    Some(head)
}

/// The SDP a compact payload stands for, rebuilt around the template both
/// peers share. `None` for anything that is not exactly a compact payload —
/// including deflated payloads from older peers, which the caller then
/// inflates as before.
pub fn expand(bytes: &[u8]) -> Option<String> {
    let mut b = bytes;
    if *take(&mut b, 1)?.first()? != MAGIC {
        return None;
    }
    let setup = *SETUPS.get(*take(&mut b, 1)?.first()? as usize)?;
    let n = *take(&mut b, 1)?.first()? as usize;
    let ufrag = std::str::from_utf8(take(&mut b, n)?).ok()?;
    let n = *take(&mut b, 1)?.first()? as usize;
    let pwd = std::str::from_utf8(take(&mut b, n)?).ok()?;
    if !ice_chars(ufrag) || !ice_chars(pwd) {
        return None;
    }
    let fp: Vec<String> = take(&mut b, 32)?
        .iter()
        .map(|x| format!("{x:02X}"))
        .collect();

    let count = *take(&mut b, 1)?.first()?;
    if count == 0 {
        return None;
    }
    let mut candidates = String::new();
    for i in 0..count {
        let tag = *take(&mut b, 1)?.first()?;
        let (addr, srflx) = match tag {
            0 => {
                let u: [u8; 16] = take(&mut b, 16)?.try_into().ok()?;
                (uuid_text(&u), false)
            }
            1 | 3 => {
                let o: [u8; 4] = take(&mut b, 4)?.try_into().ok()?;
                (Ipv4Addr::from(o).to_string(), tag == 3)
            }
            2 | 4 => {
                let o: [u8; 16] = take(&mut b, 16)?.try_into().ok()?;
                (Ipv6Addr::from(o).to_string(), tag == 4)
            }
            _ => return None,
        };
        let port = u16::from_le_bytes(take(&mut b, 2)?.try_into().ok()?);
        // Foundation and priority are ours to invent: the browser only needs
        // them distinct, and host preferred over srflx. The RFC 8445 formula,
        // minus the index so no two candidates tie.
        let pref: u32 = if srflx { 100 } else { 126 };
        let priority = ((pref << 24) | 0x00FF_FF00 | 0xFF) - u32::from(i);
        let _ = write!(
            candidates,
            "a=candidate:{} 1 udp {priority} {addr} {port} typ {}\r\n",
            i + 1,
            if srflx {
                "srflx raddr 0.0.0.0 rport 0"
            } else {
                "host"
            }
        );
    }
    // Trailing bytes mean this was never a compact payload at all.
    if !b.is_empty() {
        return None;
    }

    // The template: what headless Chromium emits, minus what only matters for
    // audio and video. `set_remote_description` is the test that keeps it
    // honest — the e2e suite connects through this exact text.
    Some(format!(
        "v=0\r\n\
         o=- 1 2 IN IP4 127.0.0.1\r\n\
         s=-\r\n\
         t=0 0\r\n\
         a=group:BUNDLE 0\r\n\
         a=msid-semantic: WMS\r\n\
         m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
         c=IN IP4 0.0.0.0\r\n\
         {candidates}\
         a=ice-ufrag:{ufrag}\r\n\
         a=ice-pwd:{pwd}\r\n\
         a=fingerprint:sha-256 {}\r\n\
         a=setup:{setup}\r\n\
         a=mid:0\r\n\
         a=sctp-port:5000\r\n\
         a=max-message-size:262144\r\n",
        fp.join(":")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from headless Chromium, srflx addresses swapped for
    /// documentation ones. The shapes are the point: mDNS host candidates,
    /// IPv4 and IPv6 srflx, and the attribute soup around them.
    const OFFER: &str = "v=0\r\n\
        o=- 2470171053369535424 2 IN IP4 127.0.0.1\r\n\
        s=-\r\n\
        t=0 0\r\n\
        a=group:BUNDLE 0\r\n\
        a=extmap-allow-mixed\r\n\
        a=msid-semantic: WMS\r\n\
        m=application 45776 UDP/DTLS/SCTP webrtc-datachannel\r\n\
        c=IN IP4 203.0.113.7\r\n\
        a=candidate:2563029213 1 udp 2113937151 1b932746-add2-4fb5-bdc5-99783097cabc.local 45776 typ host generation 0 network-cost 999\r\n\
        a=candidate:663244095 1 udp 1677732095 2001:db8::1234:5678 39612 typ srflx raddr :: rport 0 generation 0 network-cost 999\r\n\
        a=candidate:1436201420 1 udp 1677729535 203.0.113.7 45776 typ srflx raddr 0.0.0.0 rport 0 generation 0 network-cost 999\r\n\
        a=ice-ufrag:Al/p\r\n\
        a=ice-pwd:Mt8TTlbM1td+WRzFKJuXt9LQ\r\n\
        a=ice-options:trickle\r\n\
        a=fingerprint:sha-256 3A:2B:C2:7B:5C:50:F4:F2:B6:82:8E:19:9A:4F:C8:ED:FB:CB:C2:50:BF:8D:50:D3:2C:98:B7:EB:F2:97:4C:59\r\n\
        a=setup:actpass\r\n\
        a=mid:0\r\n\
        a=sctp-port:5000\r\n\
        a=max-message-size:262144\r\n";

    #[test]
    fn every_fact_survives_the_round_trip() {
        let packed = compact(OFFER).expect("a real offer was refused");
        let sdp = expand(&packed).expect("our own compact form was refused");

        for fact in [
            "a=ice-ufrag:Al/p",
            "a=ice-pwd:Mt8TTlbM1td+WRzFKJuXt9LQ",
            "a=fingerprint:sha-256 3A:2B:C2:7B",
            "a=setup:actpass",
            "1b932746-add2-4fb5-bdc5-99783097cabc.local 45776 typ host",
            "2001:db8::1234:5678 39612 typ srflx",
            "203.0.113.7 45776 typ srflx",
            "m=application 9 UDP/DTLS/SCTP webrtc-datachannel",
        ] {
            assert!(sdp.contains(fact), "lost {fact:?} in:\n{sdp}");
        }
        // And the rebuilt SDP compacts back to the same bytes: nothing the
        // format cares about lives only in the template.
        assert_eq!(compact(&sdp), Some(packed));
    }

    /// The size is the feature. If this creeps up, the QR code grows with it.
    #[test]
    fn a_real_offer_compacts_to_under_130_bytes() {
        let n = compact(OFFER).unwrap().len();
        assert!(n < 130, "compact offer is {n} bytes");
    }

    #[test]
    fn the_answers_role_is_kept_not_assumed() {
        let answer = OFFER.replace("a=setup:actpass", "a=setup:active");
        let sdp = expand(&compact(&answer).unwrap()).unwrap();
        assert!(sdp.contains("a=setup:active"));
    }

    /// Everything `compact` cannot faithfully rebuild must be refused, so the
    /// caller falls back to deflate instead of shipping a broken handshake.
    #[test]
    fn an_unrecognised_sdp_is_refused_not_mangled() {
        let sha1 = OFFER.replace("sha-256", "sha-1");
        assert!(compact(&sha1).is_none(), "unknown fingerprint accepted");
        let no_cand = OFFER
            .lines()
            .filter(|l| !l.starts_with("a=candidate"))
            .collect::<Vec<_>>()
            .join("\r\n");
        assert!(compact(&no_cand).is_none(), "candidate-free SDP accepted");
        assert!(compact("v=0\r\n").is_none());
    }

    /// TCP candidates cannot be rebuilt and are dropped; the UDP ones still
    /// go through, which is a connection rather than a fallback.
    #[test]
    fn tcp_candidates_are_dropped_udp_kept() {
        let tcp = "a=candidate:1 1 tcp 1518280447 192.0.2.1 9 typ host tcptype active\r\n";
        let both = OFFER.replace("a=ice-ufrag", &format!("{tcp}a=ice-ufrag"));
        let sdp = expand(&compact(&both).unwrap()).unwrap();
        assert!(!sdp.contains("tcp"), "a tcp candidate was rebuilt");
        assert!(sdp.contains("typ srflx"));
    }

    /// The paste trust boundary: truncations and junk are `None`, not panics,
    /// and not half an SDP.
    #[test]
    fn truncated_and_junk_bytes_are_refused() {
        let good = compact(OFFER).unwrap();
        for cut in 0..good.len() {
            assert!(expand(&good[..cut]).is_none(), "accepted {cut} bytes");
        }
        let mut long = good.clone();
        long.push(0);
        assert!(expand(&long).is_none(), "accepted a trailing byte");
        assert!(expand(&[]).is_none());
        assert!(expand(&[MAGIC]).is_none());
        // A deflated payload from an older peer must not expand by accident.
        assert!(expand(&[0x78, 0x9C, 1, 2, 3]).is_none());
    }

    /// A newline inside a stored field would become a forged SDP attribute
    /// when the template is rebuilt around it.
    #[test]
    fn a_field_that_could_forge_an_attribute_is_refused() {
        let mut evil = compact(OFFER).unwrap();
        // The ufrag is at offset 3 with its length at 2; overwrite a byte
        // with a carriage return.
        evil[3] = b'\r';
        assert!(expand(&evil).is_none(), "a control byte survived expand");
    }

    #[test]
    fn uuid_text_round_trips() {
        let name = "1b932746-add2-4fb5-bdc5-99783097cabc.local";
        assert_eq!(uuid_text(&uuid_of(name).unwrap()), name);
        assert!(uuid_of("not-a-uuid.local").is_none());
        assert!(
            uuid_of("1b932746add24fb5bdc599783097cabc").is_none(),
            "no suffix"
        );
    }
}

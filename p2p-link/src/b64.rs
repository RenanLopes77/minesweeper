//! base64url, unpadded. Written out rather than pulled in, because it is
//! twenty lines and the alternative is a dependency plus a feature flag to
//! stop it dragging in `std` on wasm.
//!
//! URL-safe alphabet (`-` and `_` instead of `+` and `/`) and no `=` padding,
//! so the result survives being pasted into a URL, a chat window, or a QR
//! code without anything re-encoding it.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        // 3 bytes -> 4 chars, minus however many bytes were missing.
        for i in 0..chunk.len() + 1 {
            out.push(ALPHABET[(n >> (18 - 6 * i) & 0x3F) as usize] as char);
        }
    }
    out
}

fn value(c: u8) -> Option<u32> {
    let v = match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'-' => 62,
        b'_' => 63,
        _ => return None,
    };
    Some(u32::from(v))
}

/// `None` for anything that is not valid base64url. This decodes whatever a
/// user pasted, so it must reject rather than guess.
pub fn decode(s: &str) -> Option<Vec<u8>> {
    let s = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    for chunk in s.chunks(4) {
        // A lone trailing character cannot encode even one byte.
        if chunk.len() == 1 {
            return None;
        }
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= value(c)? << (18 - 6 * i);
        }
        for i in 0..chunk.len() - 1 {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg");
        assert_eq!(encode(b"fo"), "Zm8");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foob"), "Zm9vYg");
        assert_eq!(encode(b"fooba"), "Zm9vYmE");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn alphabet_is_url_safe() {
        // 0xFB 0xFF encodes to the two characters that differ from standard
        // base64; if these ever come out as + or /, a URL will mangle them.
        let s = encode(&[0xFB, 0xFF, 0xBF]);
        assert!(
            !s.contains('+') && !s.contains('/') && !s.contains('='),
            "{s}"
        );
        assert_eq!(decode(&s).unwrap(), vec![0xFB, 0xFF, 0xBF]);
    }

    #[test]
    fn rejects_junk() {
        assert!(decode("Zm9v!").is_none(), "accepted an illegal character");
        assert!(decode("Zm9vYg=").is_none(), "accepted padding");
        assert!(decode("Z").is_none(), "accepted a lone character");
    }

    #[test]
    fn round_trips_every_length() {
        let data: Vec<u8> = (0..=255u8).collect();
        for n in 0..data.len() {
            let slice = &data[..n];
            assert_eq!(decode(&encode(slice)).as_deref(), Some(slice), "len {n}");
        }
    }
}

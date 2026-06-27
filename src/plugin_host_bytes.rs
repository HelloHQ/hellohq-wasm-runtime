// SPDX-License-Identifier: Apache-2.0
//
//! Base64 (RFC 4648, standard alphabet, padded) for the typed-host byte bridge.
//!
//! The typed `storage`/`events` WIT values are `list<u8>` (bytes), but the app's
//! plugin storage is string-based (the G2 string-only `storage_set`). Carrying
//! the bytes as a base64 STRING over the JSON host-call wire means the app's
//! existing servicer stores them unchanged (a base64 string is still a string),
//! and the runtime host transparently encodes/decodes — so the app's storage
//! layer needs no schema change and stays binary-safe. Hand-rolled (no dep) so
//! the iOS no-JIT slice carries nothing extra; covered by the round-trip + known
//! -vector tests below.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode bytes as standard padded base64.
pub(crate) fn b64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(b2 & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Decode standard base64 (padding optional; whitespace and `=` ignored).
/// Returns `None` on an invalid character or a truncated quantum.
pub(crate) fn b64_decode(input: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let symbols: Vec<u8> = input
        .bytes()
        .filter(|&c| c != b'=' && !c.is_ascii_whitespace())
        .collect();
    let mut out = Vec::with_capacity(symbols.len() / 4 * 3);
    for chunk in symbols.chunks(4) {
        if chunk.len() < 2 {
            return None; // a lone symbol can't form a byte
        }
        let v0 = val(chunk[0])?;
        let v1 = val(chunk[1])?;
        out.push((v0 << 2) | (v1 >> 4));
        if chunk.len() >= 3 {
            let v2 = val(chunk[2])?;
            out.push(((v1 & 0x0f) << 4) | (v2 >> 2));
            if chunk.len() == 4 {
                let v3 = val(chunk[3])?;
                out.push(((v2 & 0x03) << 6) | v3);
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        // RFC 4648 §10.
        for (raw, b64) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
            (b"hello", "aGVsbG8="),
        ] {
            assert_eq!(b64_encode(raw), b64, "encode {raw:?}");
            assert_eq!(b64_decode(b64).as_deref(), Some(raw), "decode {b64}");
        }
    }

    #[test]
    fn round_trips_arbitrary_bytes() {
        let data: Vec<u8> = (0u8..=255).collect();
        assert_eq!(b64_decode(&b64_encode(&data)).as_deref(), Some(&data[..]));
        // A non-UTF-8 payload survives (binary-safe).
        let bin = vec![0u8, 255, 1, 254, 0];
        assert_eq!(b64_decode(&b64_encode(&bin)).as_deref(), Some(&bin[..]));
    }

    #[test]
    fn rejects_invalid() {
        assert_eq!(b64_decode("@@@@"), None);
        assert_eq!(b64_decode("A"), None); // truncated quantum
    }
}

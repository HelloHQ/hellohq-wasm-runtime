// SPDX-License-Identifier: Apache-2.0
//! The OUT-frame format of the `wasi:http@0.3` transport
//! (`hwr_p3s_start_http` / `hwr_p3s_start_http_precompiled`): how the guest's
//! outbound request reaches the caller (the hellohq app's servicer) as
//! `HWR_P3S_OUT` frames. Plain Rust, no Wasmtime types.
//!
//! ## Format
//! Every OUT frame of an http session is one kind byte followed by its
//! payload:
//!
//! ```text
//! frame    = kind:u8 payload
//! kind     = 0x01 HEAD | 0x02 BODY | 0x03 TRAILERS
//! session  = HEAD BODY* TRAILERS? OUT_END
//! ```
//!
//! * **HEAD** (exactly one, always first) — UTF-8 text, `\n`-separated:
//!   `"{METHOD} {scheme}://{authority}{path}"`, then one `"{name}: {value}"`
//!   line per request header, then at most one reserved
//!   `x-hellohq-request-options: connect=<ns>;first-byte=<ns>;between-bytes=<ns>`
//!   line. The runtime never puts a CR/LF inside a line (see
//!   `wasi_http::encode_request_head`), so the line split is exact.
//! * **BODY** (zero or more) — raw request-body bytes, in order. Never empty.
//! * **TRAILERS** (at most one, always last) — UTF-8
//!   `"{name}=<hex(value)>;..."`, the guest's request trailer fields.
//!
//! The caller dispatches on the kind byte; it never inspects a payload to
//! decide what a frame is. That is the point: the previous format recognised
//! the trailers frame by a text prefix (`x-hellohq-request-trailers:`), so a
//! request body whose last chunk began with that text was misread as trailers.
//!
//! A frame boundary is the transport's own (one `HWR_P3S_OUT` event per frame,
//! read via `hwr_p3s_out_ptr`/`hwr_p3s_out_len`), so no length prefix is
//! needed.
//!
//! ## Versioning
//! The first byte of the first frame is `0x01`. A runtime from before this
//! format (≤ v0.0.2) starts its head with the ASCII method name, so a caller
//! can tell the two apart from that one byte and refuse a mismatched library
//! rather than misparse it. The inbound (response) direction is unchanged.
//!
//! [`parse_request_frames`] is the reference parser — the contract the app's
//! Dart parser (`parseHttpRequestFrames`) implements.

/// Frame kind byte: the request head.
pub const KIND_HEAD: u8 = 0x01;
/// Frame kind byte: a chunk of the request body.
pub const KIND_BODY: u8 = 0x02;
/// Frame kind byte: the guest's request trailer fields.
pub const KIND_TRAILERS: u8 = 0x03;

/// What an OUT frame carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutFrameKind {
    /// [`KIND_HEAD`].
    Head,
    /// [`KIND_BODY`].
    Body,
    /// [`KIND_TRAILERS`].
    Trailers,
}

impl OutFrameKind {
    /// The kind byte for this frame kind.
    pub const fn tag(self) -> u8 {
        match self {
            OutFrameKind::Head => KIND_HEAD,
            OutFrameKind::Body => KIND_BODY,
            OutFrameKind::Trailers => KIND_TRAILERS,
        }
    }

    /// The frame kind for a kind byte, or `None` for an unknown byte.
    pub const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            KIND_HEAD => Some(OutFrameKind::Head),
            KIND_BODY => Some(OutFrameKind::Body),
            KIND_TRAILERS => Some(OutFrameKind::Trailers),
            _ => None,
        }
    }
}

/// Build one OUT frame: the kind byte, then `payload`.
pub fn encode(kind: OutFrameKind, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 1);
    frame.push(kind.tag());
    frame.extend_from_slice(payload);
    frame
}

/// Split one OUT frame into its kind and payload. `None` for an empty frame
/// or an unknown kind byte.
pub fn decode(frame: &[u8]) -> Option<(OutFrameKind, &[u8])> {
    let (&tag, payload) = frame.split_first()?;
    Some((OutFrameKind::from_tag(tag)?, payload))
}

/// Why a frame sequence is not a well-formed request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// No frames at all.
    Empty,
    /// The frame at this index is empty or has an unknown kind byte. At index
    /// 0 this is also what a pre-tag (≤ v0.0.2) runtime's head looks like.
    UnknownKind(usize),
    /// The first frame is not a HEAD.
    MissingHead,
    /// A second HEAD at this index.
    DuplicateHead(usize),
    /// A frame at this index after the TRAILERS frame.
    AfterTrailers(usize),
}

/// A request reassembled from its OUT frames by [`parse_request_frames`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestFrames {
    /// The HEAD payload (UTF-8 text; see the module docs).
    pub head: Vec<u8>,
    /// Every BODY payload, concatenated in order.
    pub body: Vec<u8>,
    /// The TRAILERS payload, if the request had trailers.
    pub trailers: Option<Vec<u8>>,
}

/// The reference parser: reassemble a request from its OUT frames, enforcing
/// `HEAD BODY* TRAILERS?`.
pub fn parse_request_frames<'a, I>(frames: I) -> Result<RequestFrames, FrameError>
where
    I: IntoIterator<Item = &'a [u8]>,
{
    let mut out = RequestFrames::default();
    let mut seen_any = false;
    for (index, frame) in frames.into_iter().enumerate() {
        seen_any = true;
        let (kind, payload) = decode(frame).ok_or(FrameError::UnknownKind(index))?;
        if out.trailers.is_some() {
            return Err(FrameError::AfterTrailers(index));
        }
        match (index, kind) {
            (0, OutFrameKind::Head) => out.head = payload.to_vec(),
            (0, _) => return Err(FrameError::MissingHead),
            (_, OutFrameKind::Head) => return Err(FrameError::DuplicateHead(index)),
            (_, OutFrameKind::Body) => out.body.extend_from_slice(payload),
            (_, OutFrameKind::Trailers) => out.trailers = Some(payload.to_vec()),
        }
    }
    if seen_any {
        Ok(out)
    } else {
        Err(FrameError::Empty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(fs: &[Vec<u8>]) -> Result<RequestFrames, FrameError> {
        parse_request_frames(fs.iter().map(Vec::as_slice))
    }

    #[test]
    fn kind_bytes_are_stable() {
        // The wire contract with the app: never renumber.
        assert_eq!((KIND_HEAD, KIND_BODY, KIND_TRAILERS), (0x01, 0x02, 0x03));
        for kind in [
            OutFrameKind::Head,
            OutFrameKind::Body,
            OutFrameKind::Trailers,
        ] {
            assert_eq!(OutFrameKind::from_tag(kind.tag()), Some(kind));
        }
        assert_eq!(OutFrameKind::from_tag(0x00), None);
        assert_eq!(OutFrameKind::from_tag(b'G'), None);
    }

    #[test]
    fn encode_decode_roundtrip() {
        let f = encode(OutFrameKind::Body, b"\x00\x01binary");
        assert_eq!(f[0], KIND_BODY);
        assert_eq!(
            decode(&f),
            Some((OutFrameKind::Body, &b"\x00\x01binary"[..]))
        );
        // An empty payload still has its kind byte.
        assert_eq!(encode(OutFrameKind::Head, b""), vec![KIND_HEAD]);
        assert_eq!(decode(&[]), None);
        assert_eq!(decode(&[0x7f, 1, 2]), None);
    }

    #[test]
    fn head_only() {
        let r = frames(&[encode(OutFrameKind::Head, b"GET https://a.example/")]).unwrap();
        assert_eq!(r.head, b"GET https://a.example/");
        assert!(r.body.is_empty());
        assert_eq!(r.trailers, None);
    }

    #[test]
    fn head_body_trailers() {
        let r = frames(&[
            encode(OutFrameKind::Head, b"POST https://a.example/x"),
            encode(OutFrameKind::Body, b"req-"),
            encode(OutFrameKind::Body, b"body"),
            encode(OutFrameKind::Trailers, b"x-trace=31"),
        ])
        .unwrap();
        assert_eq!(r.body, b"req-body");
        assert_eq!(r.trailers.as_deref(), Some(&b"x-trace=31"[..]));
    }

    /// THE regression: a body chunk that starts with the old trailers prefix
    /// is body, because its kind byte says so.
    #[test]
    fn body_starting_with_the_old_trailers_prefix_is_body() {
        let tricky = b"x-hellohq-request-trailers: x-trace=31".to_vec();
        let r = frames(&[
            encode(OutFrameKind::Head, b"POST https://a.example/x"),
            encode(OutFrameKind::Body, b"first;"),
            encode(OutFrameKind::Body, &tricky),
        ])
        .unwrap();
        let mut expected = b"first;".to_vec();
        expected.extend_from_slice(&tricky);
        assert_eq!(r.body, expected);
        assert_eq!(r.trailers, None);

        // ...and with real trailers after it, both survive intact.
        let r = frames(&[
            encode(OutFrameKind::Head, b"POST https://a.example/x"),
            encode(OutFrameKind::Body, &tricky),
            encode(OutFrameKind::Trailers, b"x-trace=32"),
        ])
        .unwrap();
        assert_eq!(r.body, tricky);
        assert_eq!(r.trailers.as_deref(), Some(&b"x-trace=32"[..]));
    }

    /// A body chunk that looks like a kind byte + head is still body.
    #[test]
    fn body_starting_with_a_kind_byte_is_body() {
        let r = frames(&[
            encode(OutFrameKind::Head, b"POST https://a.example/x"),
            encode(OutFrameKind::Body, &[KIND_TRAILERS, b'a']),
            encode(OutFrameKind::Body, &[KIND_HEAD, b'b']),
        ])
        .unwrap();
        assert_eq!(r.body, vec![KIND_TRAILERS, b'a', KIND_HEAD, b'b']);
        assert_eq!(r.trailers, None);
    }

    #[test]
    fn malformed_sequences_are_rejected() {
        assert_eq!(frames(&[]), Err(FrameError::Empty));
        // A pre-tag runtime's head starts with the method name.
        assert_eq!(
            frames(&[b"GET https://a.example/".to_vec()]),
            Err(FrameError::UnknownKind(0))
        );
        assert_eq!(
            frames(&[encode(OutFrameKind::Body, b"x")]),
            Err(FrameError::MissingHead)
        );
        let head = encode(OutFrameKind::Head, b"GET https://a.example/");
        assert_eq!(
            frames(&[head.clone(), head.clone()]),
            Err(FrameError::DuplicateHead(1))
        );
        assert_eq!(
            frames(&[
                head.clone(),
                encode(OutFrameKind::Trailers, b""),
                encode(OutFrameKind::Body, b"late"),
            ]),
            Err(FrameError::AfterTrailers(2))
        );
        assert_eq!(frames(&[head, Vec::new()]), Err(FrameError::UnknownKind(1)));
    }
}

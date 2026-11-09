// SPDX-License-Identifier: Apache-2.0
//
//! Wire format between an app and the `hellohq-wasm-host` helper process.
//!
//! The helper runs ONE plugin session out of the app's process and relays the
//! same events the C ABI's poll/resolve and poll/push round-trips produce. It
//! adds no behaviour: every frame maps onto one `hwr_p3_*` / `hwr_p3s_*` call,
//! so an app can swap the in-process FFI for the helper without changing what a
//! plugin sees.
//!
//! Every frame is `[u32 LE length][u8 type][payload]`, where `length` counts the
//! type byte plus the payload. Both directions use the same framing.
//!
//! ```text
//! app → helper                         helper → app
//!   START   (once, first)                READY   (once, before START is read)
//!   RESOLVE (answers a PENDING)          PENDING (a host call awaits a value)
//!   PUSH    (an inbound stream chunk)    OUT     (an outbound stream chunk)
//!   PUSH_END(inbound stream closed)      OUT_END (outbound stream finished)
//!                                        DONE    (run finished; the result)
//!                                        ERROR   (run failed; UTF-8 message)
//! ```
//!
//! One session per process: after DONE or ERROR the helper exits. The app
//! cancels a run by ending the process; the helper also exits on its own when
//! its stdin closes, so a crashed app never leaves it running.

/// Bumped on any change to frame types or payload layouts.
pub const PROTOCOL_VERSION: u32 = 1;

/// Largest frame either side accepts (type byte + payload). A component or a
/// result larger than this is refused rather than buffered.
pub const MAX_FRAME_LEN: u32 = 256 * 1024 * 1024;

// app → helper
pub const FRAME_START: u8 = 0x01;
pub const FRAME_RESOLVE: u8 = 0x02;
pub const FRAME_PUSH: u8 = 0x03;
pub const FRAME_PUSH_END: u8 = 0x04;

// helper → app
pub const FRAME_READY: u8 = 0x80;
pub const FRAME_PENDING: u8 = 0x81;
pub const FRAME_DONE: u8 = 0x82;
pub const FRAME_ERROR: u8 = 0x83;
pub const FRAME_OUT: u8 = 0x84;
pub const FRAME_OUT_END: u8 = 0x85;

/// Which session START opens. Each maps onto one C ABI start function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionKind {
    /// `hwr_p3_start_plugin_compile`: a raw SDK plugin component (Cranelift).
    PluginCompile = 1,
    /// `hwr_p3_start_plugin`: a precompiled plugin artifact.
    PluginPrecompiled = 2,
    /// `hwr_p3s_start_http`: a `wasi:http` guest on the streaming transport.
    Http = 3,
    /// `hwr_p3s_start_inference`: an `ai:inference` guest on the same transport.
    Inference = 4,
}

impl SessionKind {
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            1 => Some(Self::PluginCompile),
            2 => Some(Self::PluginPrecompiled),
            3 => Some(Self::Http),
            4 => Some(Self::Inference),
            _ => None,
        }
    }

    /// True for the poll/push streaming transport (`hwr_p3s_*`).
    pub fn is_stream(self) -> bool {
        matches!(self, Self::Http | Self::Inference)
    }
}

/// The START payload:
/// `[u8 kind][u8 use_pulley][u64 LE fuel][u32 LE component_len][component]`
/// `[u32 LE input_len][input]`. `fuel` 0 keeps the runtime default; `input` is
/// ignored by the streaming kinds.
#[derive(Debug, PartialEq, Eq)]
pub struct Start {
    pub kind: SessionKind,
    pub use_pulley: bool,
    pub fuel: u64,
    pub component: Vec<u8>,
    pub input: Vec<u8>,
}

impl Start {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(22 + self.component.len() + self.input.len());
        out.push(self.kind as u8);
        out.push(self.use_pulley as u8);
        out.extend_from_slice(&self.fuel.to_le_bytes());
        out.extend_from_slice(&(self.component.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.component);
        out.extend_from_slice(&(self.input.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.input);
        out
    }

    pub fn decode(payload: &[u8]) -> Result<Self, String> {
        let mut r = Reader {
            buf: payload,
            pos: 0,
        };
        let kind = r.u8()?;
        let kind = SessionKind::from_byte(kind).ok_or(format!("unknown session kind {kind}"))?;
        let use_pulley = match r.u8()? {
            0 => false,
            1 => true,
            other => return Err(format!("bad use_pulley flag {other}")),
        };
        let fuel = r.u64()?;
        let component = r.bytes()?.to_vec();
        let input = r.bytes()?.to_vec();
        if r.pos != payload.len() {
            return Err("trailing bytes after START".into());
        }
        Ok(Start {
            kind,
            use_pulley,
            fuel,
            component,
            input,
        })
    }
}

/// The READY payload: `[u32 LE protocol version][u32 LE runtime C ABI version]`.
pub fn encode_ready(abi_version: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    out.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    out.extend_from_slice(&abi_version.to_le_bytes());
    out
}

/// Write one frame. Errors only if the payload is too large or the write fails.
pub fn write_frame(w: &mut impl std::io::Write, ty: u8, payload: &[u8]) -> std::io::Result<()> {
    let len = payload.len() as u64 + 1;
    if len > u64::from(MAX_FRAME_LEN) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "frame larger than MAX_FRAME_LEN",
        ));
    }
    w.write_all(&(len as u32).to_le_bytes())?;
    w.write_all(&[ty])?;
    w.write_all(payload)?;
    w.flush()
}

/// Read one frame. `Ok(None)` is a clean end of input between frames; a stream
/// that ends inside a frame, or a frame over [MAX_FRAME_LEN], is an error.
pub fn read_frame(r: &mut impl std::io::Read) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len);
    if len == 0 || len > MAX_FRAME_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("bad frame length {len}"),
        ));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    let ty = body[0];
    body.remove(0);
    Ok(Some((ty, body)))
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.buf.len())
            .ok_or("START payload is truncated")?;
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn bytes(&mut self) -> Result<&'a [u8], String> {
        let n = u32::from_le_bytes(self.take(4)?.try_into().unwrap()) as usize;
        self.take(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_round_trips() {
        let s = Start {
            kind: SessionKind::Inference,
            use_pulley: true,
            fuel: 42,
            component: vec![1, 2, 3],
            input: b"in".to_vec(),
        };
        assert_eq!(Start::decode(&s.encode()).unwrap(), s);
    }

    #[test]
    fn start_rejects_truncation_trailing_bytes_and_unknown_kinds() {
        let good = Start {
            kind: SessionKind::PluginCompile,
            use_pulley: false,
            fuel: 0,
            component: vec![9; 10],
            input: vec![],
        }
        .encode();
        for cut in 0..good.len() {
            assert!(
                Start::decode(&good[..cut]).is_err(),
                "accepted a cut at {cut}"
            );
        }
        let mut extra = good.clone();
        extra.push(0);
        assert!(Start::decode(&extra).is_err());
        let mut bad_kind = good.clone();
        bad_kind[0] = 9;
        assert!(Start::decode(&bad_kind).is_err());
        let mut bad_flag = good;
        bad_flag[1] = 2;
        assert!(Start::decode(&bad_flag).is_err());
    }

    #[test]
    fn frames_round_trip_and_eof_is_clean_only_between_frames() {
        let mut buf = Vec::new();
        write_frame(&mut buf, FRAME_OUT, b"abc").unwrap();
        write_frame(&mut buf, FRAME_OUT_END, b"").unwrap();
        let mut r = std::io::Cursor::new(buf.clone());
        assert_eq!(
            read_frame(&mut r).unwrap(),
            Some((FRAME_OUT, b"abc".to_vec()))
        );
        assert_eq!(read_frame(&mut r).unwrap(), Some((FRAME_OUT_END, vec![])));
        assert_eq!(read_frame(&mut r).unwrap(), None);

        // Cut inside the first frame: an error, not a clean end.
        let mut cut = std::io::Cursor::new(buf[..6].to_vec());
        assert!(read_frame(&mut cut).is_err());
    }

    #[test]
    fn oversized_and_empty_frames_are_refused() {
        let mut over = Vec::new();
        over.extend_from_slice(&(MAX_FRAME_LEN + 1).to_le_bytes());
        assert!(read_frame(&mut std::io::Cursor::new(over)).is_err());
        let zero = 0u32.to_le_bytes().to_vec();
        assert!(read_frame(&mut std::io::Cursor::new(zero)).is_err());
    }
}

// SPDX-License-Identifier: Apache-2.0
//! A hand-built `wasi:http@0.3-rc` host. STAGE 1 built the resource host
//! scaffolding (the non-streaming surface: method / headers / status /
//! options); STAGE 3 (this file's current state) lands the **working streaming
//! core + `handler::handle`**, proven end-to-end against a real guest component
//! (`tests/http_handle.rs`).
//!
//! - The four stream-minting methods (`request::new` / `response::new` /
//!   `request::consume_body` / `response::consume_body`) are SYNC WIT funcs that
//!   nonetheless mint `stream<u8>` / `future<…>` values. They're flagged `store`
//!   in `bindgen!` (NOT `async` — see the filter strings below) so they receive
//!   a `wasmtime::component::Access` (a scoped store borrow) in place of `&mut
//!   self`, letting them mint streams via `StreamReader::new`.
//! - `handler::handle` is `async func` in the WIT → already concurrent
//!   (`Accessor`-based). STAGE 3 synthesizes the response IN-PROCESS: status
//!   200, an `x-hellohq: ok` header, and a body echoing `"{method}
//!   {authority}{path}"`. STAGE 4 replaces the synthetic response with a real
//!   P3 round-trip to Dart's gated fetch.
//!
//! Requires `Config::concurrency_support(true)` (to mint streams/futures).
//! Gated behind the `wasi-http` feature (via the `mod` decl in `lib.rs`); not
//! part of normal builds.

use std::collections::HashMap;
use std::pin::Pin;
use std::task::{Context, Poll};

wasmtime::component::bindgen!({
    path: "wit-wasi",
    world: "http-probe",
    imports: {
        "wasi:http/types@0.3.0-rc-2026-01-06.[static]request.new": store,
        "wasi:http/types@0.3.0-rc-2026-01-06.[static]request.consume-body": store,
        "wasi:http/types@0.3.0-rc-2026-01-06.[static]response.new": store,
        "wasi:http/types@0.3.0-rc-2026-01-06.[static]response.consume-body": store,
        default: async,
    },
});

// Pull the generated types into scope. The generated module tree is
// `wasi::http::types::*` / `wasi::http::handler::*` / `wasi::clocks::types::*`.
use wasi::clocks::types::Duration;
use wasi::http::types::{
    ErrorCode, FieldName, FieldValue, Fields, HeaderError, Method, Request, RequestOptions,
    RequestOptionsError, Response, Scheme, StatusCode,
};

// ─── In-memory backing data for each resource kind ──────────────────────────

/// Backing state for a `fields` (a.k.a. `headers` / `trailers`) resource.
///
/// Headers are stored as an ordered list of `(name, value)` pairs — names may
/// repeat (multi-valued headers), matching the WIT `list<tuple<field-name,
/// field-value>>` shape. Names compare case-insensitively (HTTP semantics);
/// values are raw bytes.
#[derive(Debug, Default, Clone)]
struct FieldsData {
    entries: Vec<(String, Vec<u8>)>,
    /// `true` once this `fields` has been handed out as immutable (e.g. via
    /// `request.get-headers`), per the WIT: such handles fail mutating ops with
    /// `header-error.immutable`.
    immutable: bool,
}

impl FieldsData {
    /// Case-insensitive name match (HTTP header names are case-insensitive).
    fn matches(name: &str, candidate: &str) -> bool {
        name.eq_ignore_ascii_case(candidate)
    }

    fn get(&self, name: &str) -> Vec<FieldValue> {
        self.entries
            .iter()
            .filter(|(n, _)| Self::matches(name, n))
            .map(|(_, v)| v.clone())
            .collect()
    }

    fn has(&self, name: &str) -> bool {
        self.entries.iter().any(|(n, _)| Self::matches(name, n))
    }

    /// Clears existing values for `name` and inserts the new ones.
    fn set(&mut self, name: &str, values: Vec<FieldValue>) {
        self.entries.retain(|(n, _)| !Self::matches(name, n));
        for v in values {
            self.entries.push((name.to_string(), v));
        }
    }

    fn delete(&mut self, name: &str) {
        self.entries.retain(|(n, _)| !Self::matches(name, n));
    }

    fn get_and_delete(&mut self, name: &str) -> Vec<FieldValue> {
        let removed = self.get(name);
        self.delete(name);
        removed
    }

    fn append(&mut self, name: &str, value: FieldValue) {
        self.entries.push((name.to_string(), value));
    }
}

/// The trailers future shape shared by `request`/`response`:
/// `future<result<option<trailers>, error-code>>`.
type TrailersFuture = wasmtime::component::FutureReader<
    Result<Option<wasmtime::component::Resource<Fields>>, ErrorCode>,
>;

/// Backing state for a `request` resource. STAGE 3 additionally stashes the
/// incoming body stream + trailers future minted by the guest in `request::new`,
/// so `consume_body` can hand them back (the host echo path reads metadata, not
/// the body — STAGE 4's P3 transport will consume the real stream).
#[derive(Debug, Default)]
struct RequestData {
    method: MethodOwned,
    path_with_query: Option<String>,
    scheme: Option<SchemeOwned>,
    authority: Option<String>,
    /// rep of the backing `fields` for this request's headers.
    headers: u32,
    /// rep of the optional backing `request-options`, if any.
    options: Option<u32>,
    /// The incoming body content stream (`none` = zero-length body), stashed in
    /// `request::new` and handed back by `consume_body`.
    body: Option<wasmtime::component::StreamReader<u8>>,
    /// The incoming trailers future, stashed in `request::new`.
    trailers: Option<TrailersFuture>,
}

/// Backing state for a `response` resource. STAGE 3 stashes the body stream +
/// trailers future so `consume_body` can return them (the guest reads the
/// synthesized response body the host minted in `handler::handle`).
#[derive(Debug)]
struct ResponseData {
    status_code: StatusCode,
    headers: u32,
    /// The response body content stream (`none` = zero-length body).
    body: Option<wasmtime::component::StreamReader<u8>>,
    /// The response trailers future.
    trailers: Option<TrailersFuture>,
}

/// Backing state for a `request-options` resource: the three optional timeouts.
#[derive(Debug, Default, Clone)]
struct RequestOptionsData {
    connect_timeout: Option<Duration>,
    first_byte_timeout: Option<Duration>,
    between_bytes_timeout: Option<Duration>,
    /// `true` if handed out immutable (e.g. via `request.get-options`).
    immutable: bool,
}

// The generated `Method`/`Scheme` enums are not `Default` and carry a `String`
// in their `Other` arm, so we keep owned mirrors for storage and convert at the
// trait boundary. Defaults follow the WIT: `request.new` defaults to GET.

/// Owned mirror of the generated `Method` (defaults to GET, per `request.new`).
#[derive(Debug, Clone, Default)]
enum MethodOwned {
    #[default]
    Get,
    Head,
    Post,
    Put,
    Delete,
    Connect,
    Options,
    Trace,
    Patch,
    Other(String),
}

impl From<Method> for MethodOwned {
    fn from(m: Method) -> Self {
        match m {
            Method::Get => MethodOwned::Get,
            Method::Head => MethodOwned::Head,
            Method::Post => MethodOwned::Post,
            Method::Put => MethodOwned::Put,
            Method::Delete => MethodOwned::Delete,
            Method::Connect => MethodOwned::Connect,
            Method::Options => MethodOwned::Options,
            Method::Trace => MethodOwned::Trace,
            Method::Patch => MethodOwned::Patch,
            Method::Other(s) => MethodOwned::Other(s),
        }
    }
}

impl From<&MethodOwned> for Method {
    fn from(m: &MethodOwned) -> Self {
        match m {
            MethodOwned::Get => Method::Get,
            MethodOwned::Head => Method::Head,
            MethodOwned::Post => Method::Post,
            MethodOwned::Put => Method::Put,
            MethodOwned::Delete => Method::Delete,
            MethodOwned::Connect => Method::Connect,
            MethodOwned::Options => Method::Options,
            MethodOwned::Trace => Method::Trace,
            MethodOwned::Patch => Method::Patch,
            MethodOwned::Other(s) => Method::Other(s.clone()),
        }
    }
}

/// Owned mirror of the generated `Scheme`.
#[derive(Debug, Clone)]
enum SchemeOwned {
    Http,
    Https,
    Other(String),
}

impl From<Scheme> for SchemeOwned {
    fn from(s: Scheme) -> Self {
        match s {
            Scheme::Http => SchemeOwned::Http,
            Scheme::Https => SchemeOwned::Https,
            Scheme::Other(o) => SchemeOwned::Other(o),
        }
    }
}

impl From<&SchemeOwned> for Scheme {
    fn from(s: &SchemeOwned) -> Self {
        match s {
            SchemeOwned::Http => Scheme::Http,
            SchemeOwned::Https => Scheme::Https,
            SchemeOwned::Other(o) => Scheme::Other(o.clone()),
        }
    }
}

// ─── Host state ─────────────────────────────────────────────────────────────

/// A simple `HashMap<u32, T>` resource table keyed by `Resource::rep()`, with a
/// monotonic id allocator. We use this rather than `ResourceTable` because the
/// generated `new`/`drop` signatures hand us bare `Resource<T>` values whose
/// `rep()` we control on the way out (`Resource::new_own(rep)`) — a plain map is
/// the minimal backing store and keeps STAGE-1 dependency-free (no `slab`).
#[derive(Debug)]
struct Table<T> {
    items: HashMap<u32, T>,
    next: u32,
}

impl<T> Default for Table<T> {
    fn default() -> Self {
        Table {
            items: HashMap::new(),
            next: 1, // start at 1; 0 is reserved as a sentinel "unset" rep.
        }
    }
}

impl<T> Table<T> {
    /// Insert `value`, returning the freshly allocated rep (the `Resource` id).
    fn insert(&mut self, value: T) -> u32 {
        let rep = self.next;
        self.next += 1;
        self.items.insert(rep, value);
        rep
    }

    fn get(&self, rep: u32) -> Option<&T> {
        self.items.get(&rep)
    }

    fn get_mut(&mut self, rep: u32) -> Option<&mut T> {
        self.items.get_mut(&rep)
    }

    fn remove(&mut self, rep: u32) -> Option<T> {
        self.items.remove(&rep)
    }
}

/// STAGE 4 transport: the P3 v2 streaming channel pair the `handle` driver uses
/// to round-trip the request/response with the caller (Dart). `out` carries the
/// request head (and, later, request body) OUT to the caller; `inbound` is the
/// caller's response stream — taken (`Option::take`) and awaited by `handle`
/// OUTSIDE any `accessor.with` borrow. Absent (`transport: None`) on the
/// in-process synthetic path that `tests/http_handle.rs` exercises.
pub(crate) struct HttpTransport {
    pub(crate) out: crate::P3sOut,
    pub(crate) inbound: Option<crate::P3sIn>,
}

/// In-memory host state implementing the `wasi:http@0.3-rc` host traits. Holds a
/// resource table per resource kind, each keyed by `Resource::rep()`. STAGE 4
/// optionally carries a [`HttpTransport`]: when `Some`, `handler::handle` routes
/// through the P3 v2 transport (a gated fetch the caller services); when `None`,
/// it keeps synthesizing the in-process echo response.
#[derive(Default)]
pub struct WasiHttpHost {
    fields: Table<FieldsData>,
    requests: Table<RequestData>,
    responses: Table<ResponseData>,
    options: Table<RequestOptionsData>,
    /// STAGE 4 P3 transport; `None` on the synthetic in-process path.
    pub(crate) transport: Option<HttpTransport>,
}

impl WasiHttpHost {
    /// Construct an empty host (synthetic in-process path, `transport: None`).
    pub fn new() -> Self {
        Self::default()
    }

    /// STAGE 4: construct a host wired to a P3 v2 streaming transport, so
    /// `handler::handle` routes the request OUT to the caller and builds the
    /// response from the caller's inbound frames.
    pub(crate) fn with_transport(out: crate::P3sOut, inbound: crate::P3sIn) -> Self {
        WasiHttpHost {
            transport: Some(HttpTransport {
                out,
                inbound: Some(inbound),
            }),
            ..Default::default()
        }
    }

    /// Register every `wasi:http@0.3-rc` import (clocks types, http types, http
    /// handler) into `linker`, backed by this host type. Uses the generated
    /// world `add_to_linker` with `HasSelf<WasiHttpHost>`, so the `Store`'s data
    /// `T` *is* `WasiHttpHost` and the `&mut WasiHttpHost` blanket trait impls
    /// supply the bindings. COMPILES + links in STAGE 1; the streaming methods
    /// it wires are STAGE-3 stubs (see below).
    pub fn add_to_linker(
        linker: &mut wasmtime::component::Linker<WasiHttpHost>,
    ) -> wasmtime::Result<()> {
        HttpProbe::add_to_linker::<WasiHttpHost, wasmtime::component::HasSelf<WasiHttpHost>>(
            linker,
            |state| state,
        )
    }

    /// Register the `wasi:http@0.3-rc` imports into a linker whose store state
    /// `T` is NOT `WasiHttpHost` itself but EMBEDS one (reachable via `get`).
    /// This is what lets a richer store state — e.g. one that ALSO carries a
    /// `CapstoneHarness` to satisfy a plugin's `hellohq:plugin/*` capability
    /// imports — provide `wasi:http` from the SAME store. It is the host-side
    /// analogue of the production `plugin` world importing both the typed
    /// capabilities AND `wasi:http/handler` (wit/world.wit): one component, one
    /// store, both import sets satisfied. Mirrors
    /// [`crate::capstone::CapstoneHarness::add_to_linker_get`].
    pub fn add_to_linker_get<T: Send + 'static>(
        linker: &mut wasmtime::component::Linker<T>,
        get: fn(&mut T) -> &mut WasiHttpHost,
    ) -> wasmtime::Result<()> {
        HttpProbe::add_to_linker::<T, wasmtime::component::HasSelf<WasiHttpHost>>(linker, get)
    }
}

// ─── wasi::clocks::types::Host — trivial (only the `duration` type alias) ────

impl wasi::clocks::types::Host for WasiHttpHost {}

// ─── wasi::http::types::HostFields ───────────────────────────────────────────

impl wasi::http::types::HostFields for WasiHttpHost {
    async fn new(&mut self) -> wasmtime::component::Resource<Fields> {
        let rep = self.fields.insert(FieldsData::default());
        wasmtime::component::Resource::new_own(rep)
    }

    async fn from_list(
        &mut self,
        entries: Vec<(FieldName, FieldValue)>,
    ) -> Result<wasmtime::component::Resource<Fields>, HeaderError> {
        // STAGE 1 accepts any entries (no forbidden-header / syntax validation
        // yet — that belongs with the real outbound path). Multi-valued names
        // are preserved as repeated entries, per the WIT.
        let data = FieldsData {
            entries,
            immutable: false,
        };
        let rep = self.fields.insert(data);
        Ok(wasmtime::component::Resource::new_own(rep))
    }

    async fn get(
        &mut self,
        self_: wasmtime::component::Resource<Fields>,
        name: FieldName,
    ) -> Vec<FieldValue> {
        self.fields
            .get(self_.rep())
            .map(|f| f.get(&name))
            .unwrap_or_default()
    }

    async fn has(&mut self, self_: wasmtime::component::Resource<Fields>, name: FieldName) -> bool {
        self.fields
            .get(self_.rep())
            .map(|f| f.has(&name))
            .unwrap_or(false)
    }

    async fn set(
        &mut self,
        self_: wasmtime::component::Resource<Fields>,
        name: FieldName,
        value: Vec<FieldValue>,
    ) -> Result<(), HeaderError> {
        let Some(f) = self.fields.get_mut(self_.rep()) else {
            return Err(HeaderError::Immutable);
        };
        if f.immutable {
            return Err(HeaderError::Immutable);
        }
        f.set(&name, value);
        Ok(())
    }

    async fn delete(
        &mut self,
        self_: wasmtime::component::Resource<Fields>,
        name: FieldName,
    ) -> Result<(), HeaderError> {
        let Some(f) = self.fields.get_mut(self_.rep()) else {
            return Err(HeaderError::Immutable);
        };
        if f.immutable {
            return Err(HeaderError::Immutable);
        }
        f.delete(&name);
        Ok(())
    }

    async fn get_and_delete(
        &mut self,
        self_: wasmtime::component::Resource<Fields>,
        name: FieldName,
    ) -> Result<Vec<FieldValue>, HeaderError> {
        let Some(f) = self.fields.get_mut(self_.rep()) else {
            return Err(HeaderError::Immutable);
        };
        if f.immutable {
            return Err(HeaderError::Immutable);
        }
        Ok(f.get_and_delete(&name))
    }

    async fn append(
        &mut self,
        self_: wasmtime::component::Resource<Fields>,
        name: FieldName,
        value: FieldValue,
    ) -> Result<(), HeaderError> {
        let Some(f) = self.fields.get_mut(self_.rep()) else {
            return Err(HeaderError::Immutable);
        };
        if f.immutable {
            return Err(HeaderError::Immutable);
        }
        f.append(&name, value);
        Ok(())
    }

    async fn copy_all(
        &mut self,
        self_: wasmtime::component::Resource<Fields>,
    ) -> Vec<(FieldName, FieldValue)> {
        self.fields
            .get(self_.rep())
            .map(|f| f.entries.clone())
            .unwrap_or_default()
    }

    async fn clone(
        &mut self,
        self_: wasmtime::component::Resource<Fields>,
    ) -> wasmtime::component::Resource<Fields> {
        // Deep copy; the clone is mutable regardless of the source's flag.
        let entries = self
            .fields
            .get(self_.rep())
            .map(|f| f.entries.clone())
            .unwrap_or_default();
        let rep = self.fields.insert(FieldsData {
            entries,
            immutable: false,
        });
        wasmtime::component::Resource::new_own(rep)
    }

    async fn drop(&mut self, rep: wasmtime::component::Resource<Fields>) -> wasmtime::Result<()> {
        self.fields.remove(rep.rep());
        Ok(())
    }
}

// ─── wasi::http::types::HostRequest ──────────────────────────────────────────

impl wasi::http::types::HostRequest for WasiHttpHost {
    async fn get_method(&mut self, self_: wasmtime::component::Resource<Request>) -> Method {
        self.requests
            .get(self_.rep())
            .map(|r| Method::from(&r.method))
            .unwrap_or(Method::Get)
    }

    async fn set_method(
        &mut self,
        self_: wasmtime::component::Resource<Request>,
        method: Method,
    ) -> Result<(), ()> {
        match self.requests.get_mut(self_.rep()) {
            Some(r) => {
                r.method = method.into();
                Ok(())
            }
            None => Err(()),
        }
    }

    async fn get_path_with_query(
        &mut self,
        self_: wasmtime::component::Resource<Request>,
    ) -> Option<String> {
        self.requests
            .get(self_.rep())
            .and_then(|r| r.path_with_query.clone())
    }

    async fn set_path_with_query(
        &mut self,
        self_: wasmtime::component::Resource<Request>,
        path_with_query: Option<String>,
    ) -> Result<(), ()> {
        match self.requests.get_mut(self_.rep()) {
            Some(r) => {
                r.path_with_query = path_with_query;
                Ok(())
            }
            None => Err(()),
        }
    }

    async fn get_scheme(
        &mut self,
        self_: wasmtime::component::Resource<Request>,
    ) -> Option<Scheme> {
        self.requests
            .get(self_.rep())
            .and_then(|r| r.scheme.as_ref().map(Scheme::from))
    }

    async fn set_scheme(
        &mut self,
        self_: wasmtime::component::Resource<Request>,
        scheme: Option<Scheme>,
    ) -> Result<(), ()> {
        match self.requests.get_mut(self_.rep()) {
            Some(r) => {
                r.scheme = scheme.map(SchemeOwned::from);
                Ok(())
            }
            None => Err(()),
        }
    }

    async fn get_authority(
        &mut self,
        self_: wasmtime::component::Resource<Request>,
    ) -> Option<String> {
        self.requests
            .get(self_.rep())
            .and_then(|r| r.authority.clone())
    }

    async fn set_authority(
        &mut self,
        self_: wasmtime::component::Resource<Request>,
        authority: Option<String>,
    ) -> Result<(), ()> {
        match self.requests.get_mut(self_.rep()) {
            Some(r) => {
                r.authority = authority;
                Ok(())
            }
            None => Err(()),
        }
    }

    async fn get_options(
        &mut self,
        self_: wasmtime::component::Resource<Request>,
    ) -> Option<wasmtime::component::Resource<RequestOptions>> {
        // The WIT says the returned `request-options` is immutable; mark the
        // backing data so subsequent `set-*` ops fail with `immutable`.
        let opt_rep = self.requests.get(self_.rep()).and_then(|r| r.options);
        if let Some(rep) = opt_rep {
            if let Some(o) = self.options.get_mut(rep) {
                o.immutable = true;
            }
            Some(wasmtime::component::Resource::new_own(rep))
        } else {
            None
        }
    }

    async fn get_headers(
        &mut self,
        self_: wasmtime::component::Resource<Request>,
    ) -> wasmtime::component::Resource<Fields> {
        // The returned `headers` is immutable per the WIT — flag the backing
        // fields so `set`/`append`/`delete` fail with `header-error.immutable`.
        let hdr_rep = self
            .requests
            .get(self_.rep())
            .map(|r| r.headers)
            .unwrap_or(0);
        if let Some(f) = self.fields.get_mut(hdr_rep) {
            f.immutable = true;
        }
        wasmtime::component::Resource::new_own(hdr_rep)
    }

    async fn drop(&mut self, rep: wasmtime::component::Resource<Request>) -> wasmtime::Result<()> {
        self.requests.remove(rep.rep());
        Ok(())
    }
}

// ─── wasi::http::types::HostRequestWithStore — the stream-minting methods ─────
//
// `[static]request.new` and `[static]request.consume-body` are SYNC WIT funcs,
// but they mint `stream<u8>` / `future<...>` values — which need a `Store` (+
// `Config::concurrency_support`). The `store` bindgen filter (NOT `async`,
// since the WIT funcs aren't `async func`) gives these a `wasmtime::component::
// Access` handle in place of `&mut self`: `Access` is a scoped store borrow
// (`AsContextMut`) plus host-state access (`.get()`), so we can both mint
// streams (`StreamReader::new(&mut host, …)`) and touch the resource tables.
// `async | store` would instead generate a *concurrent* (`func_wrap_concurrent`)
// import whose ASYNC type-flag (`true`) mismatches the sync WIT func's
// (`false`) — failing instantiation with "type mismatch with async". So `store`
// alone is the correct flag for these sync-but-stream-minting methods.

impl wasi::http::types::HostRequestWithStore for wasmtime::component::HasSelf<WasiHttpHost> {
    fn new<T>(
        mut host: wasmtime::component::Access<T, Self>,
        headers: wasmtime::component::Resource<Fields>,
        contents: Option<wasmtime::component::StreamReader<u8>>,
        trailers: TrailersFuture,
        options: Option<wasmtime::component::Resource<RequestOptions>>,
    ) -> (
        wasmtime::component::Resource<Request>,
        wasmtime::component::FutureReader<Result<(), ErrorCode>>,
    ) {
        // Mint the transmission-result future (it borrows the store); a request
        // constructed in-process transmits successfully → ready `Ok(())`. The
        // producer's outer `Result<_, E>` is the host error channel (unused).
        let transmit = wasmtime::component::FutureReader::new(&mut host, async {
            Ok::<_, wasmtime::Error>(Ok::<(), ErrorCode>(()))
        })
        .expect("concurrency support enabled (Config::concurrency_support(true))");

        // Record metadata + stash the incoming body stream / trailers future so
        // a later `consume_body` (or STAGE 4's transport) can use them.
        let state = host.get();
        let data = RequestData {
            headers: headers.rep(),
            options: options.as_ref().map(|o| o.rep()),
            body: contents,
            trailers: Some(trailers),
            ..Default::default()
        };
        let rep = state.requests.insert(data);
        (
            wasmtime::component::Resource::<Request>::new_own(rep),
            transmit,
        )
    }

    fn consume_body<T>(
        mut host: wasmtime::component::Access<T, Self>,
        this: wasmtime::component::Resource<Request>,
        res: wasmtime::component::FutureReader<Result<(), ErrorCode>>,
    ) -> (wasmtime::component::StreamReader<u8>, TrailersFuture) {
        // `res` is the caller's error channel for the body transfer; the
        // in-process echo path has nothing to signal through it, so discard it.
        let _ = res;

        // Take any stashed body stream + trailers future out of the request.
        let (body, trailers) = host
            .get()
            .requests
            .get_mut(this.rep())
            .map(|r| (r.body.take(), r.trailers.take()))
            .unwrap_or((None, None));

        // Fall back to an empty body (zero-length) / ready `Ok(None)` trailers
        // when the request carried none.
        let body = body.unwrap_or_else(|| {
            wasmtime::component::StreamReader::new(&mut host, Vec::<u8>::new())
                .expect("concurrency support enabled")
        });
        let trailers = trailers.unwrap_or_else(|| {
            wasmtime::component::FutureReader::new(&mut host, async {
                Ok::<_, wasmtime::Error>(Ok::<_, ErrorCode>(None))
            })
            .expect("concurrency support enabled")
        });
        (body, trailers)
    }
}

// ─── wasi::http::types::HostResponse ─────────────────────────────────────────

impl wasi::http::types::HostResponse for WasiHttpHost {
    async fn get_status_code(
        &mut self,
        self_: wasmtime::component::Resource<Response>,
    ) -> StatusCode {
        self.responses
            .get(self_.rep())
            .map(|r| r.status_code)
            .unwrap_or(200)
    }

    async fn set_status_code(
        &mut self,
        self_: wasmtime::component::Resource<Response>,
        status_code: StatusCode,
    ) -> Result<(), ()> {
        // Reject obviously-invalid codes (outside the 100..=599 HTTP range).
        if !(100..=599).contains(&status_code) {
            return Err(());
        }
        match self.responses.get_mut(self_.rep()) {
            Some(r) => {
                r.status_code = status_code;
                Ok(())
            }
            None => Err(()),
        }
    }

    async fn get_headers(
        &mut self,
        self_: wasmtime::component::Resource<Response>,
    ) -> wasmtime::component::Resource<Fields> {
        let hdr_rep = self
            .responses
            .get(self_.rep())
            .map(|r| r.headers)
            .unwrap_or(0);
        if let Some(f) = self.fields.get_mut(hdr_rep) {
            f.immutable = true;
        }
        wasmtime::component::Resource::new_own(hdr_rep)
    }

    async fn drop(&mut self, rep: wasmtime::component::Resource<Response>) -> wasmtime::Result<()> {
        self.responses.remove(rep.rep());
        Ok(())
    }
}

// ─── wasi::http::types::HostResponseWithStore — stream-minting methods ────────

impl wasi::http::types::HostResponseWithStore for wasmtime::component::HasSelf<WasiHttpHost> {
    fn new<T>(
        mut host: wasmtime::component::Access<T, Self>,
        headers: wasmtime::component::Resource<Fields>,
        contents: Option<wasmtime::component::StreamReader<u8>>,
        trailers: TrailersFuture,
    ) -> (
        wasmtime::component::Resource<Response>,
        wasmtime::component::FutureReader<Result<(), ErrorCode>>,
    ) {
        let transmit = wasmtime::component::FutureReader::new(&mut host, async {
            Ok::<_, wasmtime::Error>(Ok::<(), ErrorCode>(()))
        })
        .expect("concurrency support enabled (Config::concurrency_support(true))");

        let state = host.get();
        let data = ResponseData {
            status_code: 200,
            headers: headers.rep(),
            body: contents,
            trailers: Some(trailers),
        };
        let rep = state.responses.insert(data);
        (
            wasmtime::component::Resource::<Response>::new_own(rep),
            transmit,
        )
    }

    fn consume_body<T>(
        mut host: wasmtime::component::Access<T, Self>,
        this: wasmtime::component::Resource<Response>,
        res: wasmtime::component::FutureReader<Result<(), ErrorCode>>,
    ) -> (wasmtime::component::StreamReader<u8>, TrailersFuture) {
        // `res` (caller error channel) is unused on the in-process echo path.
        let _ = res;
        let (body, trailers) = host
            .get()
            .responses
            .get_mut(this.rep())
            .map(|r| (r.body.take(), r.trailers.take()))
            .unwrap_or((None, None));

        let body = body.unwrap_or_else(|| {
            wasmtime::component::StreamReader::new(&mut host, Vec::<u8>::new())
                .expect("concurrency support enabled")
        });
        let trailers = trailers.unwrap_or_else(|| {
            wasmtime::component::FutureReader::new(&mut host, async {
                Ok::<_, wasmtime::Error>(Ok::<_, ErrorCode>(None))
            })
            .expect("concurrency support enabled")
        });
        (body, trailers)
    }
}

// ─── wasi::http::types::HostRequestOptions ───────────────────────────────────

impl wasi::http::types::HostRequestOptions for WasiHttpHost {
    async fn new(&mut self) -> wasmtime::component::Resource<RequestOptions> {
        let rep = self.options.insert(RequestOptionsData::default());
        wasmtime::component::Resource::new_own(rep)
    }

    async fn get_connect_timeout(
        &mut self,
        self_: wasmtime::component::Resource<RequestOptions>,
    ) -> Option<Duration> {
        self.options
            .get(self_.rep())
            .and_then(|o| o.connect_timeout)
    }

    async fn set_connect_timeout(
        &mut self,
        self_: wasmtime::component::Resource<RequestOptions>,
        duration: Option<Duration>,
    ) -> Result<(), RequestOptionsError> {
        let Some(o) = self.options.get_mut(self_.rep()) else {
            return Err(RequestOptionsError::Immutable);
        };
        if o.immutable {
            return Err(RequestOptionsError::Immutable);
        }
        o.connect_timeout = duration;
        Ok(())
    }

    async fn get_first_byte_timeout(
        &mut self,
        self_: wasmtime::component::Resource<RequestOptions>,
    ) -> Option<Duration> {
        self.options
            .get(self_.rep())
            .and_then(|o| o.first_byte_timeout)
    }

    async fn set_first_byte_timeout(
        &mut self,
        self_: wasmtime::component::Resource<RequestOptions>,
        duration: Option<Duration>,
    ) -> Result<(), RequestOptionsError> {
        let Some(o) = self.options.get_mut(self_.rep()) else {
            return Err(RequestOptionsError::Immutable);
        };
        if o.immutable {
            return Err(RequestOptionsError::Immutable);
        }
        o.first_byte_timeout = duration;
        Ok(())
    }

    async fn get_between_bytes_timeout(
        &mut self,
        self_: wasmtime::component::Resource<RequestOptions>,
    ) -> Option<Duration> {
        self.options
            .get(self_.rep())
            .and_then(|o| o.between_bytes_timeout)
    }

    async fn set_between_bytes_timeout(
        &mut self,
        self_: wasmtime::component::Resource<RequestOptions>,
        duration: Option<Duration>,
    ) -> Result<(), RequestOptionsError> {
        let Some(o) = self.options.get_mut(self_.rep()) else {
            return Err(RequestOptionsError::Immutable);
        };
        if o.immutable {
            return Err(RequestOptionsError::Immutable);
        }
        o.between_bytes_timeout = duration;
        Ok(())
    }

    async fn clone(
        &mut self,
        self_: wasmtime::component::Resource<RequestOptions>,
    ) -> wasmtime::component::Resource<RequestOptions> {
        // Deep copy; the clone is mutable regardless of the source's flag.
        let mut data = self.options.get(self_.rep()).cloned().unwrap_or_default();
        data.immutable = false;
        let rep = self.options.insert(data);
        wasmtime::component::Resource::new_own(rep)
    }

    async fn drop(
        &mut self,
        rep: wasmtime::component::Resource<RequestOptions>,
    ) -> wasmtime::Result<()> {
        self.options.remove(rep.rep());
        Ok(())
    }
}

// ─── Supertrait: wasi::http::types::Host ─────────────────────────────────────

impl wasi::http::types::Host for WasiHttpHost {}

// ─── wasi::http::handler — the async `handle` ────────────────────────────────

// The handler `Host` trait is empty; the work lives on `HostWithStore::handle`,
// which is a *static* method on the `D` projection (`HasSelf<WasiHttpHost>`),
// taking an `Accessor`. STAGE 1 returns a clean error rather than panicking.
impl wasi::http::handler::Host for WasiHttpHost {}

impl wasi::http::handler::HostWithStore for wasmtime::component::HasSelf<WasiHttpHost> {
    async fn handle<T: Send>(
        accessor: &wasmtime::component::Accessor<T, Self>,
        request: wasmtime::component::Resource<Request>,
    ) -> Result<wasmtime::component::Resource<Response>, ErrorCode> {
        // STAGE 4: when a P3 transport is present, route the request OUT to the
        // caller (Dart's gated fetch) and build the response from what the caller
        // streams back IN. When `transport` is `None` we keep the STAGE-3
        // in-process synthetic echo (so `tests/http_handle.rs` is unaffected).
        let has_transport = accessor.with(|mut access| access.get().transport.is_some());
        if has_transport {
            return handle_via_transport(accessor, request).await;
        }

        // STAGE 3 synthetic path: read the request's metadata + headers from the
        // host resource state and echo a one-line summary back as the 200 body.
        accessor.with(|mut access| {
            let host = access.get();

            // Read the request metadata. Treat a missing request as an internal
            // error (the guest passed a stale/foreign handle).
            let Some(req) = host.requests.get(request.rep()) else {
                return Err(ErrorCode::InternalError(Some(
                    "wasi:http handler: unknown request resource".to_string(),
                )));
            };
            let method = method_str(&req.method);
            let authority = req.authority.clone().unwrap_or_default();
            let path = req
                .path_with_query
                .clone()
                .unwrap_or_else(|| "/".to_string());

            // Echo summary body: e.g. "GET example.com/".
            let summary = format!("{method} {authority}{path}");
            let body_bytes = summary.into_bytes();

            // Build the response headers as a fresh `fields` carrying our marker.
            let resp_headers = FieldsData {
                entries: vec![("x-hellohq".to_string(), b"ok".to_vec())],
                immutable: false,
            };
            let headers_rep = host.fields.insert(resp_headers);

            // Mint the response body stream from the echoed `Vec<u8>` (the
            // provided `StreamProducer for Vec<u8>` buffered-body path) and a
            // ready `Ok(None)` trailers future.
            let body = wasmtime::component::StreamReader::new(&mut access, body_bytes)
                .map_err(|e| ErrorCode::InternalError(Some(e.to_string())))?;
            let trailers: TrailersFuture =
                wasmtime::component::FutureReader::new(&mut access, async {
                    Ok::<_, wasmtime::Error>(Ok::<_, ErrorCode>(None))
                })
                .map_err(|e| ErrorCode::InternalError(Some(e.to_string())))?;

            // Construct the response resource (status 200 + headers + body).
            let host = access.get();
            let rep = host.responses.insert(ResponseData {
                status_code: 200,
                headers: headers_rep,
                body: Some(body),
                trailers: Some(trailers),
            });
            Ok(wasmtime::component::Resource::<Response>::new_own(rep))
        })
    }
}

/// STAGE 4 transport path for `handle`. Frames the request OUT to the caller
/// over `transport.out`, awaits the caller's framed response on the inbound
/// receiver, and builds the `Response` resource from it.
///
/// Borrow discipline: every `accessor.with` closure pulls owned values out
/// (cloned metadata, the `take`-n inbound receiver, minted streams) and returns
/// before any `.await`. The inbound receiver is awaited LOCALLY, outside `with`,
/// so no `Access`/store borrow is held across the await.
async fn handle_via_transport<T: Send>(
    accessor: &wasmtime::component::Accessor<T, wasmtime::component::HasSelf<WasiHttpHost>>,
    request: wasmtime::component::Resource<Request>,
) -> Result<wasmtime::component::Resource<Response>, ErrorCode> {
    // (1) Read request metadata + headers, build the head bytes, emit the head
    //     OUT, take the request-body stream (if any), and take the inbound
    //     receiver — all inside one `with`, dropping the borrow before we await.
    //     The outbound stream is NOT ended here: if there's a request body we
    //     stream its bytes out as further OUT frames first (step 1b).
    let (inbound, req_body, req_trailers) = accessor.with(|mut access| {
        let host = access.get();
        let Some(req) = host.requests.get(request.rep()) else {
            return Err(ErrorCode::InternalError(Some(
                "wasi:http handler: unknown request resource".to_string(),
            )));
        };
        let method = method_str(&req.method).to_string();
        let scheme = match req.scheme.as_ref() {
            Some(SchemeOwned::Http) => "http",
            Some(SchemeOwned::Https) => "https",
            Some(SchemeOwned::Other(s)) => s.as_str(),
            None => "https",
        }
        .to_string();
        let authority = req.authority.clone().unwrap_or_default();
        let path = req
            .path_with_query
            .clone()
            .unwrap_or_else(|| "/".to_string());
        let header_entries = host
            .fields
            .get(req.headers)
            .map(|f| f.entries.clone())
            .unwrap_or_default();

        // Frame 1 = head: "{METHOD} {scheme}://{authority}{path}" then one
        // "{name}: {value}" line per header, '\n'-separated.
        let mut head = format!("{method} {scheme}://{authority}{path}");
        for (name, value) in &header_entries {
            head.push('\n');
            head.push_str(name);
            head.push_str(": ");
            head.push_str(&String::from_utf8_lossy(value));
        }
        // Surface the request's `request-options` timeouts to the caller (which
        // enforces them; the host has no timer) as a reserved head line.
        let options_line = req
            .options
            .and_then(|rep| host.options.get(rep))
            .and_then(format_request_options_line);
        if let Some(line) = options_line {
            head.push('\n');
            head.push_str(&line);
        }

        // Take the stashed request-body stream out (so we can stream it OUT) and
        // the inbound receiver out (so we can await it without the borrow).
        let host = access.get();
        let (req_body, req_trailers) = host
            .requests
            .get_mut(request.rep())
            .map(|r| (r.body.take(), r.trailers.take()))
            .unwrap_or((None, None));
        let Some(transport) = host.transport.as_mut() else {
            return Err(ErrorCode::InternalError(Some(
                "wasi:http handler: transport missing".to_string(),
            )));
        };
        transport.out.chunk(head.into_bytes());
        let inbound = transport.inbound.take();
        Ok((inbound, req_body, req_trailers))
    })?;

    // (1b) If the request carries a body, stream it OUT frame-by-frame before
    //      ending the outbound stream. We read the guest's `stream<u8>`
    //      host-side via a `StreamConsumer` ([`RequestBodyConsumer`]) piped onto
    //      the body reader: it forwards each consumed batch through an mpsc
    //      channel which we drain here, emitting one OUT chunk per batch. When
    //      the consumer's sender drops (guest closed the body stream), the
    //      receiver yields `None` and we stop. Then we `out.end()`.
    if let Some(body) = req_body {
        let (body_tx, mut body_rx) = futures_channel::mpsc::unbounded::<Vec<u8>>();
        accessor.with(|mut access| {
            // `pipe` installs the consumer; it's driven by the same concurrent
            // executor that polls this `handle` task, so it makes progress while
            // we await `body_rx` below.
            body.pipe(&mut access, RequestBodyConsumer { tx: body_tx })
                .map_err(|e| ErrorCode::InternalError(Some(e.to_string())))
        })?;

        use futures_util::StreamExt;
        while let Some(chunk) = body_rx.next().await {
            if chunk.is_empty() {
                continue;
            }
            accessor.with(|mut access| {
                let host = access.get();
                if let Some(transport) = host.transport.as_mut() {
                    transport.out.chunk(chunk);
                }
            });
        }
    }

    // (1c) Drain the guest's REQUEST trailers future (stashed in `request::new`)
    //      and, if it yields `Ok(Some(fields))`, surface its entries OUT as a
    //      final reserved head line ([`REQUEST_TRAILERS_HEAD_KEY`]) BEFORE we
    //      end the outbound stream — so the caller (and upstream) can see the
    //      request trailers. We read the single-value future host-side via a
    //      `FutureConsumer` ([`RequestTrailersConsumer`]) piped onto it, send the
    //      item through a oneshot, and await it OUTSIDE `with` (mirrors the body
    //      drain's borrow discipline). `Ok(None)` / `Err(_)` / channel errors
    //      emit NO line (graceful — the common case, as for the other guests).
    if let Some(future) = req_trailers {
        let (tx, rx) = futures_channel::oneshot::channel();
        accessor.with(|mut access| {
            future
                .pipe(&mut access, RequestTrailersConsumer { tx: Some(tx) })
                .map_err(|e| ErrorCode::InternalError(Some(e.to_string())))
        })?;

        // Await the trailers value, but only up to a bounded number of executor
        // ticks — NOT indefinitely. A cooperating guest resolves its trailers
        // future *concurrently* with `handle` (mirroring the request body), so a
        // few yields are enough for the piped consumer to fire. The common
        // no-trailers guests, however, hold the writer until their `run` returns
        // (after `handle`), so the future never resolves during `handle`; we must
        // NOT block on it (that would deadlock). We `select` `rx` against a small
        // yield budget: if the budget expires first we emit NO line and proceed
        // (the still-pending writer is harmlessly resolved/dropped later by the
        // guest). `Ok(None)` / `Err(_)` / channel error also emit nothing.
        let resolved = await_trailers_bounded(rx).await;
        if let Some(Ok(Ok(Some(fields_rep)))) = resolved {
            // Look up the trailer fields' entries inside a fresh `with`, format
            // the reserved line, and emit it as a final OUT frame.
            let line = accessor.with(|mut access| {
                let host = access.get();
                host.fields
                    .get(fields_rep.rep())
                    .and_then(|f| format_request_trailers_line(&f.entries))
            });
            if let Some(line) = line {
                accessor.with(|mut access| {
                    let host = access.get();
                    if let Some(transport) = host.transport.as_mut() {
                        transport.out.chunk(line.into_bytes());
                    }
                });
            }
        }
    }

    // End the outbound stream: head (+ any body frames + trailers line) emitted.
    accessor.with(|mut access| {
        let host = access.get();
        if let Some(transport) = host.transport.as_mut() {
            transport.out.end();
        }
    });

    let Some(mut inbound) = inbound else {
        return Err(ErrorCode::InternalError(Some(
            "wasi:http handler: inbound stream already consumed".to_string(),
        )));
    };

    // (2) Await the caller's response OUTSIDE `with`. Frame 1 = head
    //     ("{status}\n{name}: {value}..."); subsequent frames = raw body
    //     bytes, accumulated until the stream yields `None`.
    use futures_util::StreamExt;
    let Some(head_frame) = inbound.next().await else {
        return Err(ErrorCode::InternalError(Some(
            "wasi:http handler: caller closed inbound before sending a head".to_string(),
        )));
    };
    let head = String::from_utf8_lossy(&head_frame);
    let mut lines = head.split('\n');
    let status_line = lines.next().unwrap_or("");
    let status_code: StatusCode = status_line.trim().parse().map_err(|_| {
        ErrorCode::InternalError(Some(format!(
            "wasi:http handler: malformed response status line {status_line:?}"
        )))
    })?;
    let mut resp_header_entries: Vec<(String, Vec<u8>)> = Vec::new();
    // Trailer fields the caller reported on the reserved head line, if any.
    let mut resp_trailer_entries: Option<Vec<(String, FieldValue)>> = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim();
            let value = value.trim_start();
            if name.eq_ignore_ascii_case(TRAILERS_HEAD_KEY) {
                resp_trailer_entries = Some(parse_trailers_line(value));
            } else {
                resp_header_entries.push((name.to_string(), value.as_bytes().to_vec()));
            }
        }
    }

    // (3) Build the `Response` resource (status + headers + body stream) in a
    //     fresh `with`. Rather than buffering the remaining inbound frames into
    //     a `Vec`, hand the still-open receiver to a custom `StreamProducer`
    //     ([`ResponseBodyProducer`]) that forwards each inbound chunk to the
    //     guest's `stream<u8>` as it arrives — true chunk-by-chunk streaming.
    accessor.with(|mut access| {
        let host = access.get();
        let headers_rep = host.fields.insert(FieldsData {
            entries: resp_header_entries,
            immutable: false,
        });

        // If the caller reported trailers, back them with an immutable `fields`
        // resource and resolve the trailers future with `Ok(Some(..))`; else
        // resolve `Ok(None)` (the prior, no-trailers behaviour).
        let trailers_rep: Option<u32> = resp_trailer_entries.map(|entries| {
            host.fields.insert(FieldsData {
                entries,
                immutable: true,
            })
        });

        let body =
            wasmtime::component::StreamReader::new(&mut access, ResponseBodyProducer { inbound })
                .map_err(|e| ErrorCode::InternalError(Some(e.to_string())))?;
        let trailers: TrailersFuture =
            wasmtime::component::FutureReader::new(&mut access, async move {
                Ok::<_, wasmtime::Error>(Ok::<_, ErrorCode>(
                    trailers_rep.map(wasmtime::component::Resource::new_own),
                ))
            })
            .map_err(|e| ErrorCode::InternalError(Some(e.to_string())))?;

        let host = access.get();
        let rep = host.responses.insert(ResponseData {
            status_code,
            headers: headers_rep,
            body: Some(body),
            trailers: Some(trailers),
        });
        Ok(wasmtime::component::Resource::<Response>::new_own(rep))
    })
}

/// A [`StreamProducer`] that forwards inbound P3 response-body frames to the
/// guest's `stream<u8>` chunk-by-chunk, instead of buffering the whole body
/// into a `Vec<u8>` up front. It owns the *remaining* inbound receiver (the head
/// frame was already consumed in `handle_via_transport`); each `poll_produce`
/// polls the receiver and hands one frame straight to the destination.
///
/// [`StreamProducer`]: wasmtime::component::StreamProducer
struct ResponseBodyProducer {
    inbound: crate::P3sIn,
}

// Generic over the store data `D` (like the blanket `Vec<u8>` impl) so it works
// with whatever store type `handle`'s `Accessor` carries (`Access<T, …>`), not
// just `WasiHttpHost`.
impl<D> wasmtime::component::StreamProducer<D> for ResponseBodyProducer {
    type Item = u8;
    // Match the `Vec<u8>` blanket `StreamProducer` impl: deliver each chunk by
    // moving it into a `VecBuffer<u8>` via `Destination::set_buffer`.
    type Buffer = wasmtime::component::VecBuffer<u8>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        _store: wasmtime::StoreContextMut<'a, D>,
        mut destination: wasmtime::component::Destination<'a, Self::Item, Self::Buffer>,
        _finish: bool,
    ) -> Poll<wasmtime::Result<wasmtime::component::StreamResult>> {
        use futures_util::Stream;
        use wasmtime::component::StreamResult;

        // Poll the inbound receiver for the next response-body frame. The
        // receiver registers `cx`'s waker on `Pending`, so we just propagate it.
        match Pin::new(&mut self.inbound).poll_next(cx) {
            // A frame arrived: hand its bytes to the guest's read buffer and
            // report `Completed` (more frames may follow). Mirrors the `Vec<u8>`
            // blanket impl's `dst.set_buffer(chunk.into())`.
            Poll::Ready(Some(chunk)) => {
                destination.set_buffer(chunk.into());
                Poll::Ready(Ok(StreamResult::Completed))
            }
            // Receiver closed (caller pushed end-of-stream): end the guest stream.
            Poll::Ready(None) => Poll::Ready(Ok(StreamResult::Dropped)),
            // No frame yet; the waker is registered, so just wait.
            Poll::Pending => Poll::Pending,
        }
    }
}

/// A [`StreamConsumer`] that reads a guest's request-body `stream<u8>`
/// host-side and forwards each consumed batch through an mpsc channel, so the
/// `handle` driver can emit them as OUT frames. Created in `handle_via_transport`
/// and piped onto the request-body reader; the matching receiver is drained by
/// the driver. When this consumer is dropped (guest closed the body stream), its
/// sender drops and the driver's receiver yields `None`.
///
/// [`StreamConsumer`]: wasmtime::component::StreamConsumer
struct RequestBodyConsumer {
    tx: futures_channel::mpsc::UnboundedSender<Vec<u8>>,
}

impl<D> wasmtime::component::StreamConsumer<D> for RequestBodyConsumer {
    type Item = u8;

    fn poll_consume(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        mut store: wasmtime::StoreContextMut<D>,
        mut source: wasmtime::component::Source<Self::Item>,
        finish: bool,
    ) -> Poll<wasmtime::Result<wasmtime::component::StreamResult>> {
        use wasmtime::component::StreamResult;
        use wasmtime::AsContextMut;

        // If asked to wrap up (cancel) without consuming, honor it immediately.
        if finish {
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }

        // Read everything currently available into a `Vec<u8>` (which is a
        // `ReadBuffer<u8>`), then ship it through the channel as one batch (one
        // OUT frame).
        let available = source.remaining(store.as_context_mut());
        if available == 0 {
            // Nothing to consume right now and not finishing: the source is
            // drained for this write. Treat as end-of-stream for our purposes.
            return Poll::Ready(Ok(StreamResult::Dropped));
        }
        let mut buffer: Vec<u8> = Vec::with_capacity(available);
        source
            .read(store.as_context_mut(), &mut buffer)
            .map_err(|e| anyhow_msg(format!("request body read failed: {e}")))?;

        let _ = self.tx.unbounded_send(buffer);

        // More may follow; report `Completed` (we consumed ≥1 item).
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

/// A [`FutureConsumer`] that reads the guest's request-trailers `future<result<
/// option<trailers>, error-code>>` host-side and forwards the single resolved
/// value through a oneshot channel, so the `handle` driver can surface it OUT.
/// Mirrors [`RequestBodyConsumer`] but for a single value: it reads the one item
/// from the `source` into a `Vec`, sends it, and reports completion. If asked to
/// `finish` without consuming, it honors that immediately (sending nothing →
/// the driver's receiver errors → no trailers line, graceful).
///
/// [`FutureConsumer`]: wasmtime::component::FutureConsumer
struct RequestTrailersConsumer {
    tx: Option<
        futures_channel::oneshot::Sender<
            Result<Option<wasmtime::component::Resource<Fields>>, ErrorCode>,
        >,
    >,
}

impl<D> wasmtime::component::FutureConsumer<D> for RequestTrailersConsumer {
    type Item = Result<Option<wasmtime::component::Resource<Fields>>, ErrorCode>;

    fn poll_consume(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        mut store: wasmtime::StoreContextMut<D>,
        mut source: wasmtime::component::Source<'_, Self::Item>,
        finish: bool,
    ) -> Poll<wasmtime::Result<()>> {
        use wasmtime::AsContextMut;

        // Asked to wrap up without consuming: drop our sender (→ the driver's
        // receiver errors → no trailers line). Graceful.
        if finish {
            return Poll::Ready(Ok(()));
        }

        // Read the single available value into a buffer, exactly like
        // `RequestBodyConsumer` reads `Vec<u8>` — here the item is the trailers
        // result. A future yields at most one value.
        let available = source.remaining(store.as_context_mut());
        if available == 0 {
            // Nothing to consume right now and not finishing.
            return Poll::Ready(Ok(()));
        }
        let mut buffer: Vec<Self::Item> = Vec::with_capacity(available);
        source
            .read(store.as_context_mut(), &mut buffer)
            .map_err(|e| anyhow_msg(format!("request trailers read failed: {e}")))?;

        if let (Some(tx), Some(value)) = (self.tx.take(), buffer.into_iter().next()) {
            let _ = tx.send(value);
        }
        Poll::Ready(Ok(()))
    }
}

/// Await the request-trailers oneshot `rx`, but only up to a bounded number of
/// executor ticks. Returns `Some(value)` if `rx` resolved within the budget (or
/// errored — `Some(Err(..))`), or `None` if the budget expired first (the guest
/// deferred the trailers write past `handle`, the common no-trailers case).
///
/// This avoids blocking `handle` indefinitely on a future a guest may only
/// resolve after `handle` returns, while still giving a cooperating guest (which
/// resolves its trailers future concurrently, like the request body) ample ticks
/// for the piped consumer to fire. We `select` `rx` against a counter future
/// that yields `YIELD_BUDGET` times then resolves.
async fn await_trailers_bounded(
    rx: futures_channel::oneshot::Receiver<
        Result<Option<wasmtime::component::Resource<Fields>>, ErrorCode>,
    >,
) -> Option<
    Result<
        Result<Option<wasmtime::component::Resource<Fields>>, ErrorCode>,
        futures_channel::oneshot::Canceled,
    >,
> {
    // A cooperating guest's spawned writer resolves within a tick or two; this
    // budget is generous headroom while staying strictly bounded.
    const YIELD_BUDGET: usize = 64;

    let mut remaining = YIELD_BUDGET;
    let budget = std::future::poll_fn(move |cx| {
        if remaining == 0 {
            Poll::Ready(())
        } else {
            remaining -= 1;
            // Re-schedule so the executor keeps driving the piped consumer (and
            // any guest task) before we re-poll.
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    });

    use futures_util::future::{select, Either};
    futures_util::pin_mut!(budget);
    match select(rx, budget).await {
        Either::Left((value, _budget)) => Some(value),
        Either::Right(((), _rx)) => None,
    }
}

/// Build a `wasmtime::Error` from a message string (avoids pulling in `anyhow`
/// macros for the few error sites in this module).
fn anyhow_msg(msg: String) -> wasmtime::Error {
    wasmtime::Error::msg(msg)
}

/// Stable string form of a `MethodOwned` for the echo summary.
fn method_str(m: &MethodOwned) -> &str {
    match m {
        MethodOwned::Get => "GET",
        MethodOwned::Head => "HEAD",
        MethodOwned::Post => "POST",
        MethodOwned::Put => "PUT",
        MethodOwned::Delete => "DELETE",
        MethodOwned::Connect => "CONNECT",
        MethodOwned::Options => "OPTIONS",
        MethodOwned::Trace => "TRACE",
        MethodOwned::Patch => "PATCH",
        MethodOwned::Other(s) => s,
    }
}

// ─── Transport head-frame extensions: request options + response trailers ────
//
// The P3 transport frames a request OUT (head + body) to the gated caller
// (Dart's fetch) and frames the response back IN (head + body). The head is a
// `\n`-separated set of `name: value` lines. Two `wasi:http@0.3-rc` features
// ride that head as reserved lines, so no new frame type / Dart-side signal is
// needed and a caller that ignores them degrades to the prior behaviour:
//
//   * REQUEST OPTIONS (OUT head): the request's `request-options` timeouts are
//     SURFACED to the caller, which OWNS enforcement — the in-process executor
//     has no timer, so the host cannot enforce wall-clock timeouts itself.
//   * RESPONSE TRAILERS (IN head): the caller reports the upstream response's
//     trailers, which the host resolves the response trailers future with
//     (instead of the prior hard-coded `Ok(None)`).

/// Reserved OUT head line carrying the request's timeouts (nanoseconds) for the
/// caller to enforce: `x-hellohq-request-options: connect=<ns>;first-byte=<ns>;
/// between-bytes=<ns>` (only the set timeouts appear). `None` when none is set.
const REQUEST_OPTIONS_HEAD_KEY: &str = "x-hellohq-request-options";

/// Reserved IN head line carrying response trailer fields:
/// `x-hellohq-trailers: <name>=<hex(value)>;...`. Values are hex-encoded so
/// arbitrary bytes survive the `;`/`=`/`\n`-delimited head framing.
const TRAILERS_HEAD_KEY: &str = "x-hellohq-trailers";

/// Reserved OUT head line carrying the guest's REQUEST trailer fields (drained
/// from the request's trailers future) so the caller (and upstream) can see
/// them: `x-hellohq-request-trailers: <name>=<hex(value)>;...`. Distinct from
/// the response [`TRAILERS_HEAD_KEY`] (which rides the IN head). Values are
/// hex-encoded so arbitrary bytes survive the `;`/`=`/`\n` head framing.
const REQUEST_TRAILERS_HEAD_KEY: &str = "x-hellohq-request-trailers";

/// Format the [`REQUEST_OPTIONS_HEAD_KEY`] line for `opts`, or `None` if no
/// timeout is set (so no line is emitted). `Duration` is `u64` nanoseconds.
fn format_request_options_line(opts: &RequestOptionsData) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(ns) = opts.connect_timeout {
        parts.push(format!("connect={ns}"));
    }
    if let Some(ns) = opts.first_byte_timeout {
        parts.push(format!("first-byte={ns}"));
    }
    if let Some(ns) = opts.between_bytes_timeout {
        parts.push(format!("between-bytes={ns}"));
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!("{REQUEST_OPTIONS_HEAD_KEY}: {}", parts.join(";")))
}

/// Parse the [`TRAILERS_HEAD_KEY`] line value (`<name>=<hex>;...`) into trailer
/// field entries. Malformed pairs (no `=`, bad hex) are skipped.
fn parse_trailers_line(value: &str) -> Vec<(String, FieldValue)> {
    let mut out = Vec::new();
    for pair in value.split(';') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        if let Some((name, hex)) = pair.split_once('=') {
            if let Some(bytes) = hex_decode(hex.trim()) {
                out.push((name.trim().to_string(), bytes));
            }
        }
    }
    out
}

/// Encode bytes as a lowercase hex string — the inverse of [`hex_decode`].
fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Format the [`REQUEST_TRAILERS_HEAD_KEY`] line for `entries` (the guest's
/// request trailer fields), or `None` if empty (so no line is emitted). Values
/// are hex-encoded, mirroring [`parse_trailers_line`]'s decode so the Dart side
/// hex-decodes them: `x-hellohq-request-trailers: <name>=<hex(value)>;...`.
fn format_request_trailers_line(entries: &[(String, FieldValue)]) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    let parts: Vec<String> = entries
        .iter()
        .map(|(name, value)| format!("{name}={}", hex_encode(value)))
        .collect();
    Some(format!("{REQUEST_TRAILERS_HEAD_KEY}: {}", parts.join(";")))
}

/// Decode a lowercase/uppercase hex string to bytes; `None` on odd length or a
/// non-hex digit.
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if !b.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(b.len() / 2);
    let mut i = 0;
    while i < b.len() {
        let hi = (b[i] as char).to_digit(16)?;
        let lo = (b[i + 1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
        i += 2;
    }
    Some(out)
}

// ─── Unit tests (non-streaming surface) ──────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use wasi::http::types::{HostFields, HostRequest, HostRequestOptions, HostResponse};

    // Drive the async host trait methods directly with a trivial block_on; the
    // non-streaming methods never touch a store, so this exercises the real
    // logic without any guest/Accessor plumbing.
    fn run<F: std::future::Future>(f: F) -> F::Output {
        pollster::block_on(f)
    }

    #[test]
    fn fields_set_get_has_delete_roundtrip() {
        let mut host = WasiHttpHost::new();
        run(async {
            let f = HostFields::new(&mut host).await;
            let f2: wasmtime::component::Resource<Fields> =
                wasmtime::component::Resource::new_own(f.rep());

            HostFields::set(
                &mut host,
                f,
                "Content-Type".into(),
                vec![b"text/plain".to_vec()],
            )
            .await
            .unwrap();

            // Case-insensitive name lookup.
            let f3 = wasmtime::component::Resource::new_own(f2.rep());
            let got = HostFields::get(&mut host, f3, "content-type".into()).await;
            assert_eq!(got, vec![b"text/plain".to_vec()]);

            let f4 = wasmtime::component::Resource::new_own(f2.rep());
            assert!(HostFields::has(&mut host, f4, "CONTENT-TYPE".into()).await);

            let f5 = wasmtime::component::Resource::new_own(f2.rep());
            HostFields::delete(&mut host, f5, "content-type".into())
                .await
                .unwrap();
            let f6 = wasmtime::component::Resource::new_own(f2.rep());
            assert!(!HostFields::has(&mut host, f6, "content-type".into()).await);
        });
    }

    #[test]
    fn fields_append_multi_valued_and_copy_all() {
        let mut host = WasiHttpHost::new();
        run(async {
            let f = HostFields::new(&mut host).await;
            let rep = f.rep();
            HostFields::append(&mut host, f, "Set-Cookie".into(), b"a=1".to_vec())
                .await
                .unwrap();
            HostFields::append(
                &mut host,
                wasmtime::component::Resource::new_own(rep),
                "Set-Cookie".into(),
                b"b=2".to_vec(),
            )
            .await
            .unwrap();
            let got = HostFields::get(
                &mut host,
                wasmtime::component::Resource::new_own(rep),
                "set-cookie".into(),
            )
            .await;
            assert_eq!(got, vec![b"a=1".to_vec(), b"b=2".to_vec()]);

            let all =
                HostFields::copy_all(&mut host, wasmtime::component::Resource::new_own(rep)).await;
            assert_eq!(all.len(), 2);
        });
    }

    #[test]
    fn fields_from_list_and_clone_is_independent() {
        let mut host = WasiHttpHost::new();
        run(async {
            let f = HostFields::from_list(
                &mut host,
                vec![("X-A".into(), b"1".to_vec()), ("X-B".into(), b"2".to_vec())],
            )
            .await
            .unwrap();
            let rep = f.rep();
            let cloned =
                HostFields::clone(&mut host, wasmtime::component::Resource::new_own(rep)).await;
            let crep = cloned.rep();

            // Mutate the clone; original must be unaffected (deep copy).
            HostFields::set(&mut host, cloned, "X-A".into(), vec![b"changed".to_vec()])
                .await
                .unwrap();
            let orig = HostFields::get(
                &mut host,
                wasmtime::component::Resource::new_own(rep),
                "X-A".into(),
            )
            .await;
            assert_eq!(orig, vec![b"1".to_vec()]);
            let cl = HostFields::get(
                &mut host,
                wasmtime::component::Resource::new_own(crep),
                "X-A".into(),
            )
            .await;
            assert_eq!(cl, vec![b"changed".to_vec()]);
        });
    }

    #[test]
    fn fields_immutable_flag_rejects_mutation() {
        let mut host = WasiHttpHost::new();
        run(async {
            let f = HostFields::new(&mut host).await;
            // Simulate handing out as immutable (as request.get-headers does).
            host.fields.get_mut(f.rep()).unwrap().immutable = true;
            let err = HostFields::set(&mut host, f, "X".into(), vec![b"y".to_vec()])
                .await
                .unwrap_err();
            assert!(matches!(err, HeaderError::Immutable));
        });
    }

    #[test]
    fn request_method_path_scheme_authority_roundtrip() {
        let mut host = WasiHttpHost::new();
        run(async {
            // Build a request via the host (headers required by request.new is
            // streaming-coupled, so create the RequestData directly here).
            let hdr = HostFields::new(&mut host).await.rep();
            let req_rep = host.requests.insert(RequestData {
                headers: hdr,
                ..Default::default()
            });
            let req = || wasmtime::component::Resource::<Request>::new_own(req_rep);

            // Default method is GET.
            assert!(matches!(
                HostRequest::get_method(&mut host, req()).await,
                Method::Get
            ));

            HostRequest::set_method(&mut host, req(), Method::Post)
                .await
                .unwrap();
            assert!(matches!(
                HostRequest::get_method(&mut host, req()).await,
                Method::Post
            ));

            HostRequest::set_method(&mut host, req(), Method::Other("PURGE".into()))
                .await
                .unwrap();
            match HostRequest::get_method(&mut host, req()).await {
                Method::Other(s) => assert_eq!(s, "PURGE"),
                other => panic!("expected Other, got {other:?}"),
            }

            HostRequest::set_path_with_query(&mut host, req(), Some("/p?q=1".into()))
                .await
                .unwrap();
            assert_eq!(
                HostRequest::get_path_with_query(&mut host, req()).await,
                Some("/p?q=1".to_string())
            );

            HostRequest::set_scheme(&mut host, req(), Some(Scheme::Https))
                .await
                .unwrap();
            assert!(matches!(
                HostRequest::get_scheme(&mut host, req()).await,
                Some(Scheme::Https)
            ));

            HostRequest::set_authority(&mut host, req(), Some("example.com".into()))
                .await
                .unwrap();
            assert_eq!(
                HostRequest::get_authority(&mut host, req()).await,
                Some("example.com".to_string())
            );

            // get_headers returns the backing fields and marks them immutable.
            let h = HostRequest::get_headers(&mut host, req()).await;
            assert_eq!(h.rep(), hdr);
            assert!(host.fields.get(hdr).unwrap().immutable);
        });
    }

    #[test]
    fn response_status_code_roundtrip_and_validation() {
        let mut host = WasiHttpHost::new();
        run(async {
            let hdr = HostFields::new(&mut host).await.rep();
            let resp_rep = host.responses.insert(ResponseData {
                status_code: 200,
                headers: hdr,
                body: None,
                trailers: None,
            });
            let resp = || wasmtime::component::Resource::<Response>::new_own(resp_rep);

            // Default 200.
            assert_eq!(HostResponse::get_status_code(&mut host, resp()).await, 200);

            HostResponse::set_status_code(&mut host, resp(), 404)
                .await
                .unwrap();
            assert_eq!(HostResponse::get_status_code(&mut host, resp()).await, 404);

            // Out-of-range code rejected.
            assert!(HostResponse::set_status_code(&mut host, resp(), 999)
                .await
                .is_err());
        });
    }

    #[test]
    fn request_options_timeouts_roundtrip_and_immutability() {
        let mut host = WasiHttpHost::new();
        run(async {
            let o = HostRequestOptions::new(&mut host).await;
            let rep = o.rep();
            let opt = || wasmtime::component::Resource::<RequestOptions>::new_own(rep);

            assert_eq!(
                HostRequestOptions::get_connect_timeout(&mut host, opt()).await,
                None
            );

            HostRequestOptions::set_connect_timeout(&mut host, opt(), Some(1_000))
                .await
                .unwrap();
            HostRequestOptions::set_first_byte_timeout(&mut host, opt(), Some(2_000))
                .await
                .unwrap();
            HostRequestOptions::set_between_bytes_timeout(&mut host, opt(), Some(3_000))
                .await
                .unwrap();

            assert_eq!(
                HostRequestOptions::get_connect_timeout(&mut host, opt()).await,
                Some(1_000)
            );
            assert_eq!(
                HostRequestOptions::get_first_byte_timeout(&mut host, opt()).await,
                Some(2_000)
            );
            assert_eq!(
                HostRequestOptions::get_between_bytes_timeout(&mut host, opt()).await,
                Some(3_000)
            );

            // Mark immutable (as request.get-options does) → sets fail.
            host.options.get_mut(rep).unwrap().immutable = true;
            let err = HostRequestOptions::set_connect_timeout(&mut host, opt(), Some(9))
                .await
                .unwrap_err();
            assert!(matches!(err, RequestOptionsError::Immutable));

            // clone() yields a mutable, independent copy.
            let cloned = HostRequestOptions::clone(&mut host, opt()).await;
            let crep = cloned.rep();
            assert_eq!(
                HostRequestOptions::get_connect_timeout(
                    &mut host,
                    wasmtime::component::Resource::new_own(crep)
                )
                .await,
                Some(1_000)
            );
            HostRequestOptions::set_connect_timeout(
                &mut host,
                wasmtime::component::Resource::new_own(crep),
                Some(42),
            )
            .await
            .unwrap();
            assert_eq!(
                HostRequestOptions::get_connect_timeout(
                    &mut host,
                    wasmtime::component::Resource::new_own(crep)
                )
                .await,
                Some(42)
            );
        });
    }

    #[test]
    fn add_to_linker_links() {
        // Proves the generated world `add_to_linker` accepts our host impl and
        // links every import (clocks types + http types + http handler).
        let mut cfg = wasmtime::Config::new();
        cfg.wasm_component_model(true);
        cfg.wasm_component_model_async(true);
        let engine = wasmtime::Engine::new(&cfg).unwrap();
        let mut linker = wasmtime::component::Linker::<WasiHttpHost>::new(&engine);
        WasiHttpHost::add_to_linker(&mut linker).expect("add_to_linker must link");
    }

    // ── Transport head-frame extensions (pure helpers) ───────────────────────

    #[test]
    fn request_options_line_omits_unset_and_emits_set() {
        // No timeout set → no line at all.
        assert_eq!(
            format_request_options_line(&RequestOptionsData::default()),
            None
        );
        // Only the set timeouts appear, in nanoseconds, in order.
        let opts = RequestOptionsData {
            connect_timeout: Some(1_000),
            between_bytes_timeout: Some(3_000),
            ..Default::default()
        };
        assert_eq!(
            format_request_options_line(&opts).as_deref(),
            Some("x-hellohq-request-options: connect=1000;between-bytes=3000")
        );
        // All three set.
        let all = RequestOptionsData {
            connect_timeout: Some(1),
            first_byte_timeout: Some(2),
            between_bytes_timeout: Some(3),
            immutable: false,
        };
        assert_eq!(
            format_request_options_line(&all).as_deref(),
            Some("x-hellohq-request-options: connect=1;first-byte=2;between-bytes=3")
        );
    }

    #[test]
    fn trailers_line_roundtrips_via_hex() {
        // hex decode basics + binary-safety (a value with ';' and '=').
        assert_eq!(hex_decode("00ff"), Some(vec![0x00, 0xff]));
        assert_eq!(hex_decode("0"), None); // odd length
        assert_eq!(hex_decode("zz"), None); // non-hex

        // "x-checksum" = "a;b=c" (bytes 61 3b 62 3d 63), "trace" = "1".
        let parsed = parse_trailers_line("x-checksum=613b623d63;trace=31");
        assert_eq!(
            parsed,
            vec![
                ("x-checksum".to_string(), b"a;b=c".to_vec()),
                ("trace".to_string(), b"1".to_vec()),
            ]
        );
        // Malformed pairs are skipped; empty input → no entries.
        assert!(parse_trailers_line("").is_empty());
        assert_eq!(
            parse_trailers_line("good=31;nobad;odd=1"),
            vec![("good".to_string(), b"1".to_vec())]
        );
    }

    #[test]
    fn hex_encode_roundtrips_with_decode() {
        assert_eq!(hex_encode(&[0x00, 0xff]), "00ff");
        assert_eq!(hex_encode(b""), "");
        // Round-trip: encode then decode is the identity (binary-safe).
        let bytes = b"a;b=c".to_vec();
        assert_eq!(hex_decode(&hex_encode(&bytes)), Some(bytes));
    }

    #[test]
    fn request_trailers_line_omits_empty_and_emits_entries() {
        // No entries → no line at all.
        assert_eq!(format_request_trailers_line(&[]), None);

        // A single entry hex-encodes its value; "req-trailer-1" → its hex.
        let entries = vec![("x-trace".to_string(), b"req-trailer-1".to_vec())];
        assert_eq!(
            format_request_trailers_line(&entries).as_deref(),
            Some(
                format!(
                    "x-hellohq-request-trailers: x-trace={}",
                    hex_encode(b"req-trailer-1")
                )
                .as_str()
            )
        );

        // Multiple entries are ';'-joined, and decode back via parse_trailers_line
        // (binary-safe — value with ';' and '=' survives).
        let entries = vec![
            ("x-checksum".to_string(), b"a;b=c".to_vec()),
            ("trace".to_string(), b"1".to_vec()),
        ];
        let line = format_request_trailers_line(&entries).unwrap();
        let value = line.strip_prefix("x-hellohq-request-trailers: ").unwrap();
        assert_eq!(parse_trailers_line(value), entries);
    }
}

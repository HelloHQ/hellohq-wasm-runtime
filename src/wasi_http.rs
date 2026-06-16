// SPDX-License-Identifier: Apache-2.0
//! STAGE 1 of a hand-built `wasi:http@0.3-rc` host: the **resource host
//! scaffolding**. An in-memory implementation of the `bindgen!`-generated host
//! traits that COMPILES (`add_to_linker` links) and unit-tests the
//! non-streaming surface (method / headers / status / options).
//!
//! The streaming body methods (`request::new` / `response::new` /
//! `consume_body`) and the async `handler::handle` are deliberately STUBBED in
//! this stage — see the `// STAGE 3: streaming` markers. The streaming core
//! (real `StreamReader<u8>` / `FutureReader<…>` bodies routed through the P3
//! round-trip to Dart's gated fetch) lands in a later stage.
//!
//! Feasibility was proven by the earlier `wasi_http_probe` module (this file is
//! its successor): `bindgen!` over the vendored `wit-wasi/` world compiles on
//! Wasmtime 45. Gated behind the `wasi-http` feature (via the `mod` decl in
//! `lib.rs`); not part of normal builds.

use std::collections::HashMap;

wasmtime::component::bindgen!({
    path: "wit-wasi",
    world: "http-probe",
    imports: { default: async },
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

/// Backing state for a `request` resource. STAGE-1 stores only the
/// non-streaming metadata; the body stream + trailers future are not retained
/// (STAGE 3 will wire them through the P3 round-trip).
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
}

/// Backing state for a `response` resource (non-streaming portion).
#[derive(Debug)]
struct ResponseData {
    status_code: StatusCode,
    headers: u32,
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

/// In-memory host state implementing the `wasi:http@0.3-rc` host traits. Holds a
/// resource table per resource kind, each keyed by `Resource::rep()`.
#[derive(Debug, Default)]
pub struct WasiHttpHost {
    fields: Table<FieldsData>,
    requests: Table<RequestData>,
    responses: Table<ResponseData>,
    options: Table<RequestOptionsData>,
}

impl WasiHttpHost {
    /// Construct an empty host.
    pub fn new() -> Self {
        Self::default()
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
    // STAGE 3: streaming. `contents` (the body `stream<u8>`) and `trailers`
    // (the trailers `future`) are accepted but NOT retained or consumed here —
    // STAGE 3 wires them through the P3 round-trip. We store only the headers +
    // options metadata, and return a trailers/result future that this stage
    // cannot construct without a store, so we hand back a never-resolving
    // placeholder via the streaming stub path. To keep `add_to_linker` linking
    // without a store handle, the returned result-future is produced by
    // `todo!()` — replaced in STAGE 3.
    async fn new(
        &mut self,
        headers: wasmtime::component::Resource<Fields>,
        contents: Option<wasmtime::component::StreamReader<u8>>,
        trailers: wasmtime::component::FutureReader<
            Result<Option<wasmtime::component::Resource<Fields>>, ErrorCode>,
        >,
        options: Option<wasmtime::component::Resource<RequestOptions>>,
    ) -> (
        wasmtime::component::Resource<Request>,
        wasmtime::component::FutureReader<Result<(), ErrorCode>>,
    ) {
        // Record the non-streaming metadata so getters round-trip.
        let data = RequestData {
            headers: headers.rep(),
            options: options.as_ref().map(|o| o.rep()),
            ..Default::default()
        };
        let rep = self.requests.insert(data);
        let _request = wasmtime::component::Resource::<Request>::new_own(rep);

        // STAGE 3: streaming. Drop the body stream + trailers future for now
        // (we cannot consume them without a `Store`/`Accessor`, which the host
        // trait signature does not provide here).
        let _ = contents;
        let _ = trailers;

        // STAGE 3: streaming. The transmission-result `future<result>` requires
        // a store to construct (`FutureReader::new` takes one). Not available in
        // this `&mut self` method, so this is deferred to STAGE 3.
        todo!("STAGE 3: request::new transmission-result future requires a store/Accessor")
    }

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

    async fn consume_body(
        &mut self,
        this: wasmtime::component::Resource<Request>,
        res: wasmtime::component::FutureReader<Result<(), ErrorCode>>,
    ) -> (
        wasmtime::component::StreamReader<u8>,
        wasmtime::component::FutureReader<
            Result<Option<wasmtime::component::Resource<Fields>>, ErrorCode>,
        >,
    ) {
        // STAGE 3: streaming. Returning an immediately-empty body stream + a
        // ready trailers future both require a `Store`/`Accessor` to build
        // (`StreamReader::new` / `FutureReader::new` take one), which this
        // `&mut self` method does not get. Deferred to the streaming stage.
        let _ = this;
        let _ = res;
        todo!(
            "STAGE 3: request::consume_body empty stream/trailers future requires a store/Accessor"
        )
    }

    async fn drop(&mut self, rep: wasmtime::component::Resource<Request>) -> wasmtime::Result<()> {
        self.requests.remove(rep.rep());
        Ok(())
    }
}

// ─── wasi::http::types::HostResponse ─────────────────────────────────────────

impl wasi::http::types::HostResponse for WasiHttpHost {
    async fn new(
        &mut self,
        headers: wasmtime::component::Resource<Fields>,
        contents: Option<wasmtime::component::StreamReader<u8>>,
        trailers: wasmtime::component::FutureReader<
            Result<Option<wasmtime::component::Resource<Fields>>, ErrorCode>,
        >,
    ) -> (
        wasmtime::component::Resource<Response>,
        wasmtime::component::FutureReader<Result<(), ErrorCode>>,
    ) {
        // Record the non-streaming metadata (default status 200, per the WIT).
        let data = ResponseData {
            status_code: 200,
            headers: headers.rep(),
        };
        let rep = self.responses.insert(data);
        let _response = wasmtime::component::Resource::<Response>::new_own(rep);

        // STAGE 3: streaming. Body stream + trailers future not retained yet.
        let _ = contents;
        let _ = trailers;
        // STAGE 3: streaming. The transmission-result future needs a store.
        todo!("STAGE 3: response::new transmission-result future requires a store/Accessor")
    }

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

    async fn consume_body(
        &mut self,
        this: wasmtime::component::Resource<Response>,
        res: wasmtime::component::FutureReader<Result<(), ErrorCode>>,
    ) -> (
        wasmtime::component::StreamReader<u8>,
        wasmtime::component::FutureReader<
            Result<Option<wasmtime::component::Resource<Fields>>, ErrorCode>,
        >,
    ) {
        // STAGE 3: streaming. Same store/Accessor limitation as request::consume_body.
        let _ = this;
        let _ = res;
        todo!("STAGE 3: response::consume_body empty stream/trailers future requires a store/Accessor")
    }

    async fn drop(&mut self, rep: wasmtime::component::Resource<Response>) -> wasmtime::Result<()> {
        self.responses.remove(rep.rep());
        Ok(())
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
        _accessor: &wasmtime::component::Accessor<T, Self>,
        _request: wasmtime::component::Resource<Request>,
    ) -> Result<wasmtime::component::Resource<Response>, ErrorCode> {
        // STAGE 3: streaming. The real outbound fetch is routed app-side
        // (gated) via the P3 round-trip and constructs the response from a
        // body stream. Until then, return a clean, non-panicking error.
        Err(ErrorCode::InternalError(Some(
            "wasi:http handler not yet implemented (STAGE 3)".to_string(),
        )))
    }
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
}

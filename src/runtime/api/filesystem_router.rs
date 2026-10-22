//! `Bun.FileSystemRouter` / `MatchedRoute` — Next.js-style file router.

use bun_paths::strings;
use std::rc::Rc;

use bun_ast as Log;
use bun_core::Utf8Bytes;
use bun_jsc::bindgen::{self, JsHeap, JsLocal, JsPayload as _, JsScope, JsWrapper, to_js};
use bun_jsc::ref_string::{RefString, StringTooLong};
use bun_jsc::virtual_machine::VirtualMachine;
use bun_jsc::{JSGlobalObject, JSValue, JsCell, JsResult, LogJsc};
use bun_paths::{self as path, MAX_PATH_BYTES};
use bun_ptr::BackRef;

use bun_http_types::URLPath;
use bun_resolver::fs as Fs;
use bun_router::{RouteConfig, Routes};
use bun_sys_jsc::ErrorJsc as _;
use bun_url::{CombinedScanner, QueryStringMap, URL, route_param};

use crate::api::bun_object;
use crate::generated_bindgen::filesystem_router::{
    FileSystemRouterOptions, Routable, RouteKind, RouterStyle, js_FileSystemRouter,
};
use crate::node::types::Valid;

const DEFAULT_EXTENSIONS: &[&[u8]] = &[b"tsx", b"jsx", b"ts", b"mjs", b"cjs", b"js"];

/// A ref on a string of the VM's interning cache.
struct RefCountedString(BackRef<RefString>);

impl RefCountedString {
    fn new(vm: &mut VirtualMachine, bytes: &[u8]) -> Result<Self, StringTooLong> {
        let string = BackRef::from(vm.ref_counted_string(bytes, None)?);
        // The cache adds no ref on a hit.
        string.ref_();
        Ok(Self(string))
    }

    fn bytes(&self) -> &[u8] {
        self.0.leak()
    }
}

impl Clone for RefCountedString {
    fn clone(&self) -> Self {
        self.0.ref_();
        Self(self.0)
    }
}

impl Drop for RefCountedString {
    fn drop(&mut self) {
        // `get()`: a bare `deref()` is `<BackRef as Deref>::deref`.
        self.0.get().deref();
    }
}

pub(crate) struct FileSystemRouter {
    origin: Option<RefCountedString>,
    asset_prefix: Option<RefCountedString>,
    /// `dir` is the directory as the caller named it: every scan follows its links again.
    config: RouteConfig,
    /// `reload()` puts a new table here and leaves the old one as it is: a `MatchedRoute` shares it.
    routes: JsCell<Rc<Routes>>,
}

/// The routes that are below `config.dir` on disk now, or `None` if there is no such directory.
/// `log` gets a message for each route file that is left out.
fn scan(
    global: &JSGlobalObject,
    config: &RouteConfig,
    log: &mut Log::Log,
) -> JsResult<Option<Routes>> {
    let dir = strings::paths::without_trailing_slash_windows_path(&config.dir);
    let scanned = match bun_sys::Dir::open(dir) {
        Ok(dir) => Routes::scan(dir, &config.extensions, log),
        Err(err) if matches!(err.get_errno(), bun_sys::E::ENOENT | bun_sys::E::ENOTDIR) => {
            return Ok(None);
        }
        Err(err) => Err(err),
    };
    match scanned {
        Ok(routes) => Ok(Some(routes)),
        Err(err) => Err(global.throw_value(err.to_js(global)?)),
    }
}

impl FileSystemRouter {
    pub(crate) fn constructor<'a>(
        cx: &mut JsScope<'a>,
        options: &FileSystemRouterOptions<'a>,
    ) -> bindgen::Result<'a, Self> {
        match options.style {
            Some(b"nextjs") => {}
            Some(_) => {
                return Err(cx.heap.invalid_arguments(format_args!(
                    "Only 'nextjs' style is currently implemented"
                )));
            }
            None => {
                return Err(cx.heap.invalid_arguments(format_args!(
                    "Expected 'style' option (ex: \"style\": \"nextjs\")"
                )));
            }
        }
        let global = cx.global();
        Valid::no_null_bytes(options.dir, "dir", "a string", global)?;

        let mut out_buf = [0u8; MAX_PATH_BYTES * 2];
        let mut root_dir_path = global.bun_vm().top_level_dir();
        if !(options.dir.is_empty() || options.dir == b".") {
            // An absolute path is joined too: `.` and `..` in it are text, as they are in a relative one.
            let Some(joined) =
                path::resolve_path::join_abs_string_buf_checked::<path::platform::Auto>(
                    Fs::FileSystem::instance().top_level_dir(),
                    &mut out_buf,
                    &[options.dir],
                )
            else {
                return Err(cx.heap.error(format_args!(
                    "Unable to find directory: {}",
                    bstr::BStr::new(options.dir)
                )));
            };
            root_dir_path = joined;
        }

        let mut extensions: Vec<Box<[u8]>> = options
            .file_extensions
            .iter()
            .flatten()
            .filter(|extension| !extension.is_empty())
            .map(|extension| Box::from(&extension[1..]))
            .collect();
        if extensions.is_empty() {
            extensions = DEFAULT_EXTENSIONS.iter().map(|s| Box::from(*s)).collect();
        }

        let config = RouteConfig {
            // Ends with the native separator (`\` on Windows).
            dir: strings::paths::clone_normalizing_separators(root_dir_path).into_boxed_slice(),
            extensions: extensions.into(),
        };
        let mut log = Log::Log::new();
        let Some(routes) = scan(global, &config, &mut log)? else {
            return Err(cx.heap.error(format_args!(
                "Unable to find directory: {}",
                bstr::BStr::new(root_dir_path)
            )));
        };
        // `reload()` leaves out a route file that has an error. A new router does not start with one.
        if log.has_errors() {
            let error = log.to_js(global, format_args!("loading routes"))?;
            return Err(global.throw_value(error).into());
        }

        let vm = global.bun_vm().as_mut();
        let mut ref_counted = |bytes: &[u8]| {
            RefCountedString::new(vm, bytes).map_err(|_| global.throw_string_too_long())
        };
        let not_empty = |bytes: &&[u8]| !bytes.is_empty();
        Ok(FileSystemRouter {
            origin: options
                .origin
                .filter(not_empty)
                .map(&mut ref_counted)
                .transpose()?,
            asset_prefix: options
                .asset_prefix
                .filter(not_empty)
                .map(&mut ref_counted)
                .transpose()?,
            config,
            routes: JsCell::new(Rc::new(routes)),
        })
    }

    pub(crate) fn get_asset_prefix(&self) -> Option<impl AsRef<[u8]>> {
        self.asset_prefix.as_ref().map(RefCountedString::bytes)
    }

    pub(crate) fn r#match<'a>(
        &self,
        heap: &JsHeap<'a>,
        input: Routable<'a>,
    ) -> bindgen::Result<'a, Option<JsWrapper<'a, MatchedRoute>>> {
        // `path` is moved into the `MatchedRoute`, so every arm must own its bytes.
        let mut path: Utf8Bytes<'static> = match input {
            Routable::Path(path) => path.into_utf8(),
            Routable::Request(req) => {
                req.ensure_url().expect("unreachable");
                req.url.get().clone().into_utf8()
            }
            Routable::Response(resp) => resp.url().clone().into_utf8(),
        };

        if path.slice().is_empty() || (path.slice().len() == 1 && path.slice()[0] == b'/') {
            path = Utf8Bytes::Borrowed(b"/");
        }

        if strings::has_prefix(path.slice(), b"http://")
            || strings::has_prefix(path.slice(), b"https://")
            || strings::has_prefix(path.slice(), b"file://")
        {
            path = Utf8Bytes::Owned(URL::parse(path.slice()).pathname.to_vec());
        }

        // URLPath::parse strips byte 0 and the route table is keyed without the
        // leading slash, so "Xtop" would match "/top". The URL branch above and
        // the empty-input normalisation both yield '/'-prefixed paths already.
        if path.slice().first() != Some(&b'/') {
            return Ok(None);
        }

        // `url_path`, `route` and `params_list` borrow `path`, or the decoded buffer that takes its place below.
        let mut url_path = match URLPath::parse(path.slice()) {
            Ok(v) => v,
            Err(err) => {
                return Err(heap.error(format_args!(
                    "{} parsing path: {}",
                    bun_url::Error::from(err).name(),
                    bstr::BStr::new(path.slice())
                )));
            }
        };
        // SAFETY: a match borrows bytes of the route table. The `MatchedRoute` below keeps the
        // match and holds the table, which an `Rc` does not move and which nothing changes.
        let routes: &'static Routes = unsafe { bun_ptr::detach_lifetime_ref(&**self.routes.get()) };
        let mut params_list = route_param::List::default();
        let Some(route) = routes.match_page_with_allocator(&url_path, &mut params_list) else {
            return Ok(None);
        };

        // If `URLPath::parse` had to percent-decode, `route.pathname`/`query_string` and
        // the param values borrow the decode buffer — not `path` — and that buffer would
        // be freed when `url_path` drops at the end of this call. Take ownership of it and
        // make it the backing allocation instead. (`Box<[u8]>` -> `Vec<u8>` ->
        // `Utf8Bytes::Owned` reuses the same heap allocation, so the borrowed slices
        // stay valid; nothing in `route` points into the original encoded `path` once a
        // decode happened.)
        if let Some(decoded) = url_path.take_decoded_storage() {
            path = Utf8Bytes::Owned(decoded.into_vec());
        }

        debug_assert!(core::ptr::eq(route.pathname, path.slice()));
        let matched = MatchedRoute {
            name: route.name,
            file_path: route.file_path,
            query_start: route.pathname.len() - route.query_string.len(),
            pathname: path,
            params_list,
            param_map: JsCell::new(None),
            origin: self.origin.clone(),
            asset_prefix: self.asset_prefix.clone(),
            routes: Rc::clone(self.routes.get()),
        };
        Ok(Some(matched.into_js(heap)))
    }

    pub(crate) fn get_origin(&self) -> Option<impl AsRef<[u8]>> {
        self.origin.as_ref().map(RefCountedString::bytes)
    }

    pub(crate) fn reload<'a>(
        &self,
        this_value: JsWrapper<'a, Self>,
        cx: &mut JsScope<'a>,
    ) -> bindgen::Result<'a, JsWrapper<'a, Self>> {
        // The messages are not read: a route file that has an error is left out.
        let mut log = Log::Log::new();
        let Some(routes) = scan(cx.global(), &self.config, &mut log)? else {
            return Err(cx.heap.error(format_args!(
                "Unable to find directory: {}",
                bstr::BStr::new(&*self.config.dir)
            )));
        };
        self.routes.set(Rc::new(routes));
        js_FileSystemRouter::routes_set_cached(this_value.to_js(), cx.global(), JSValue::ZERO);
        Ok(this_value)
    }

    pub(crate) fn get_routes(
        &self,
    ) -> impl IntoIterator<Item = (impl AsRef<[u8]>, impl AsRef<[u8]>)> {
        self.routes.get().iter()
    }

    pub(crate) fn get_style(&self) -> RouterStyle {
        RouterStyle::Nextjs
    }
}

pub(crate) struct MatchedRoute {
    /// The route's. Bytes of `routes`, as `file_path` is.
    name: &'static [u8],
    file_path: &'static [u8],
    /// Decoded, with the query string.
    pathname: Utf8Bytes<'static>,
    /// Where the query string starts in `pathname`.
    query_start: usize,
    /// Its names are bytes of `routes`, and its values borrow `pathname`.
    params_list: route_param::List<'static>,
    /// Made by the first of `params` and `query` to need it.
    param_map: JsCell<Option<QueryStringMap>>,
    origin: Option<RefCountedString>,
    asset_prefix: Option<RefCountedString>,
    /// The table the route was matched in, which a later `reload()` does not change.
    routes: Rc<Routes>,
}

impl MatchedRoute {
    fn pathname_without_leading_slash(&self) -> &[u8] {
        strings::trim_left(self.pathname.slice(), b"/")
    }

    /// A name that comes once has a string, and one that comes again has an array of them.
    fn create_query_object<'a>(
        heap: &JsHeap<'a>,
        map: Option<&QueryStringMap>,
    ) -> bindgen::Result<'a, JsLocal<'a>> {
        let Some(map) = map else {
            return Ok(heap.create_empty_object().as_value());
        };
        let mut buffer: [&[u8]; 256] = [b""; 256];
        let mut names = map.iter();
        let entries = core::iter::from_fn(|| {
            let entry = names.next(&mut buffer)?;
            let value = match &*entry.values {
                [value] => to_js::utf8(heap, value),
                values => to_js::array(heap, values, to_js::utf8),
            };
            Some((entry.name, value))
        });
        // Never cuts: it tells `record` how many names there can be.
        let entries = entries.take(map.get_name_count());
        to_js::record(heap, entries, |_, value| value)
    }
}

impl MatchedRoute {
    pub(crate) fn get_file_path(&self) -> impl AsRef<[u8]> {
        self.file_path
    }

    pub(crate) fn get_kind(&self) -> RouteKind {
        let name = self.name;
        if strings::contains(name, b"[[...") {
            RouteKind::OptionalCatchAll
        } else if strings::contains(name, b"[...") {
            RouteKind::CatchAll
        } else if strings::contains(name, b"[") {
            RouteKind::Dynamic
        } else {
            RouteKind::Exact
        }
    }

    pub(crate) fn get_name(&self) -> impl AsRef<[u8]> {
        self.name
    }

    pub(crate) fn get_params<'a>(&self, heap: &JsHeap<'a>) -> bindgen::Result<'a, JsLocal<'a>> {
        if self.params_list.is_empty() {
            return Self::create_query_object(heap, None);
        }

        if self.param_map.get().is_none() {
            let scanner = CombinedScanner::init(
                b"",
                self.pathname_without_leading_slash(),
                self.name,
                &self.params_list,
            );
            self.param_map
                .set(QueryStringMap::init_with_scanner(scanner)?);
        }

        Self::create_query_object(heap, self.param_map.get().as_ref())
    }

    pub(crate) fn get_pathname(&self) -> impl AsRef<[u8]> {
        self.pathname.slice()
    }

    pub(crate) fn get_query<'a>(&self, heap: &JsHeap<'a>) -> bindgen::Result<'a, JsLocal<'a>> {
        let query_string = &self.pathname.slice()[self.query_start..];
        if query_string.is_empty() {
            return self.get_params(heap);
        }

        let map = if self.params_list.is_empty() {
            QueryStringMap::init(query_string)?
        } else {
            QueryStringMap::init_with_scanner(CombinedScanner::init(
                query_string,
                self.pathname_without_leading_slash(),
                self.name,
                &self.params_list,
            ))?
        };
        Self::create_query_object(heap, map.as_ref())
    }

    pub(crate) fn get_script_src(&self) -> impl AsRef<[u8]> {
        self.get_src()
    }

    pub(crate) fn get_src(&self) -> impl AsRef<[u8]> {
        let mut src: Vec<u8> = Vec::new();
        let origin_url = self
            .origin
            .as_ref()
            .map(|origin| URL::parse(origin.bytes()));
        bun_object::get_public_path_with_asset_prefix(
            self.file_path,
            self.routes.dir(),
            &origin_url.unwrap_or_default(),
            self.asset_prefix
                .as_ref()
                .map_or(b"", RefCountedString::bytes),
            &mut src,
            path::Platform::Posix,
        );
        src
    }
}

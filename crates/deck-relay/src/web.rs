//! The phone app (PWA) the relay serves from its own origin (`--web-dir`,
//! `docs/notes/mobile-pwa.md` "Relay serves the PWA"): every file is read into memory at startup,
//! precompressed `.br`/`.gz` siblings included, and answered from that table with fixed security
//! headers. There is no SPA fallback and `/v1/*` never serves files.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::path::{Path, PathBuf};

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::Response;

use crate::RelayError;

/// Upper bound of everything loaded (precompressed siblings included).
pub(crate) const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
                   img-src 'self' data: blob:; font-src 'self'; connect-src 'self'; \
                   manifest-src 'self'; worker-src 'self'; base-uri 'none'; form-action 'none'; \
                   frame-ancestors 'none'";
const PERMISSIONS: &str = "camera=(), microphone=(), geolocation=(), payment=(), usb=()";
const HSTS: &str = "max-age=31536000";
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const NO_CACHE: &str = "no-cache";
/// Build output with content-hashed names (`pwa/build.ts`); everything else revalidates.
const HASHED_DIR: &str = "assets/";

/// The loaded app.
pub(crate) struct WebApp {
    files: HashMap<String, Asset>,
    /// Sends `Strict-Transport-Security` (in-process TLS).
    hsts: bool,
    total_bytes: u64,
}

struct Asset {
    content_type: &'static str,
    cache_control: &'static str,
    identity: Variant,
    br: Option<Variant>,
    gzip: Option<Variant>,
}

struct Variant {
    body: Bytes,
    etag: HeaderValue,
}

impl Variant {
    fn new(bytes: Vec<u8>) -> Variant {
        let mut h = DefaultHasher::new();
        bytes.hash(&mut h);
        let etag = format!("\"{:016x}-{:x}\"", h.finish(), bytes.len());
        Variant {
            // Hex digits and `-` only: always a valid header value.
            etag: HeaderValue::from_str(&etag).unwrap_or(HeaderValue::from_static("\"0\"")),
            body: Bytes::from(bytes),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Identity,
    Br,
    Gzip,
}

impl WebApp {
    /// Reads every file under `dir` (names starting with `.` skipped). Fails on a symlink that
    /// leads outside `dir`, a non-UTF-8 name, or more than [`MAX_TOTAL_BYTES`] in total.
    pub fn load(dir: &Path, hsts: bool) -> Result<WebApp, RelayError> {
        let root = fs::canonicalize(dir).map_err(|e| RelayError::Io(dir.to_path_buf(), e))?;
        if !root.is_dir() {
            return Err(RelayError::Web(dir.to_path_buf(), "not a directory".into()));
        }
        let mut walk = Walk {
            root: &root,
            raw: BTreeMap::new(),
            total: 0,
            ancestors: vec![root.clone()],
        };
        walk.dir(&root, "")?;
        let Walk { mut raw, total, .. } = walk;

        let mut files = HashMap::new();
        let keys: Vec<String> = raw.keys().cloned().collect();
        for key in keys {
            let is_sibling = [".br", ".gz"]
                .iter()
                .any(|ext| key.strip_suffix(ext).is_some_and(|b| raw.contains_key(b)));
            if is_sibling {
                continue;
            }
            let Some(bytes) = raw.remove(&key) else {
                continue;
            };
            let br = raw.remove(&format!("{key}.br")).map(Variant::new);
            let gzip = raw.remove(&format!("{key}.gz")).map(Variant::new);
            let asset = Asset {
                content_type: content_type(&key),
                cache_control: if key.starts_with(HASHED_DIR) {
                    IMMUTABLE
                } else {
                    NO_CACHE
                },
                identity: Variant::new(bytes),
                br,
                gzip,
            };
            files.insert(key, asset);
        }
        Ok(WebApp {
            files,
            hsts,
            total_bytes: total,
        })
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// The response for a request the explicit routes did not take.
    pub fn respond(&self, method: &Method, path: &str, headers: &HeaderMap) -> Response {
        if path == "/v1" || path.starts_with("/v1/") {
            return self.status(StatusCode::NOT_FOUND);
        }
        let asset = match path {
            "/" => self.files.get("index.html"),
            _ => path
                .strip_prefix('/')
                .and_then(percent_decode)
                .and_then(|key| self.files.get(&key)),
        };
        let Some(asset) = asset else {
            return self.status(StatusCode::NOT_FOUND);
        };
        if method != Method::GET && method != Method::HEAD {
            let mut resp = self.status(StatusCode::METHOD_NOT_ALLOWED);
            resp.headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
            return resp;
        }

        let accept = headers.get(header::ACCEPT_ENCODING);
        let (encoding, variant) = [
            (Encoding::Br, asset.br.as_ref(), "br"),
            (Encoding::Gzip, asset.gzip.as_ref(), "gzip"),
        ]
        .into_iter()
        .filter_map(|(enc, v, name)| Some((enc, v?, q_value(accept, name))))
        .filter(|&(_, _, q)| q > 0.0)
        // Strictly greater replaces: ties keep the earlier (br before gzip).
        .fold(None::<(Encoding, &Variant, f32)>, |best, cand| match best {
            Some(b) if b.2 >= cand.2 => Some(b),
            _ => Some(cand),
        })
        .map_or((Encoding::Identity, &asset.identity), |(e, v, _)| (e, v));
        let not_modified = headers
            .get(header::IF_NONE_MATCH)
            .is_some_and(|inm| etag_matches(inm, &variant.etag));
        let (status, body) = if not_modified {
            (StatusCode::NOT_MODIFIED, Body::empty())
        } else if method == Method::HEAD {
            (StatusCode::OK, Body::empty())
        } else {
            (StatusCode::OK, Body::from(variant.body.clone()))
        };
        let mut resp = Response::new(body);
        *resp.status_mut() = status;
        let h = resp.headers_mut();
        self.secure(h);
        h.insert(header::ETAG, variant.etag.clone());
        h.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(asset.cache_control),
        );
        h.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
        if status == StatusCode::OK {
            h.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static(asset.content_type),
            );
            match encoding {
                Encoding::Br => {
                    h.insert(header::CONTENT_ENCODING, HeaderValue::from_static("br"));
                }
                Encoding::Gzip => {
                    h.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
                }
                Encoding::Identity => {}
            }
            if method == Method::HEAD {
                h.insert(
                    header::CONTENT_LENGTH,
                    HeaderValue::from(variant.body.len()),
                );
            }
        }
        resp
    }

    /// A bodyless-content status answer (`404`, `405`) with the security headers.
    fn status(&self, status: StatusCode) -> Response {
        let mut resp = Response::new(Body::from(
            status
                .canonical_reason()
                .unwrap_or("error")
                .to_ascii_lowercase(),
        ));
        *resp.status_mut() = status;
        let h = resp.headers_mut();
        self.secure(h);
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        );
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static(NO_CACHE));
        resp
    }

    fn secure(&self, h: &mut HeaderMap) {
        let set = |h: &mut HeaderMap, name: header::HeaderName, value: &'static str| {
            h.insert(name, HeaderValue::from_static(value));
        };
        set(h, header::CONTENT_SECURITY_POLICY, CSP);
        set(h, header::REFERRER_POLICY, "no-referrer");
        set(h, header::X_CONTENT_TYPE_OPTIONS, "nosniff");
        set(
            h,
            header::HeaderName::from_static("cross-origin-opener-policy"),
            "same-origin",
        );
        set(
            h,
            header::HeaderName::from_static("cross-origin-resource-policy"),
            "same-origin",
        );
        set(h, header::X_FRAME_OPTIONS, "DENY");
        set(
            h,
            header::HeaderName::from_static("permissions-policy"),
            PERMISSIONS,
        );
        if self.hsts {
            set(h, header::STRICT_TRANSPORT_SECURITY, HSTS);
        }
    }
}

/// The recursive directory read behind [`WebApp::load`].
struct Walk<'a> {
    root: &'a Path,
    /// Relative `/`-separated path → contents.
    raw: BTreeMap<String, Vec<u8>>,
    total: u64,
    /// Canonical directories on the path being read (a symlink back up the chain cannot loop;
    /// a symlink to a directory read elsewhere is still served at its own path).
    ancestors: Vec<PathBuf>,
}

impl Walk<'_> {
    fn dir(&mut self, dir: &Path, prefix: &str) -> Result<(), RelayError> {
        let io = |p: &Path| {
            let p = p.to_path_buf();
            move |e| RelayError::Io(p, e)
        };
        for entry in fs::read_dir(dir).map_err(io(dir))? {
            let entry = entry.map_err(io(dir))?;
            let path = entry.path();
            let Ok(name) = entry.file_name().into_string() else {
                return Err(RelayError::Web(path, "file name is not UTF-8".into()));
            };
            if name.starts_with('.') {
                continue;
            }
            let file_type = entry.file_type().map_err(io(&path))?;
            let real = if file_type.is_symlink() {
                let target = fs::canonicalize(&path).map_err(io(&path))?;
                if !target.starts_with(self.root) {
                    return Err(RelayError::Web(
                        path,
                        "symlink leads outside the web directory".into(),
                    ));
                }
                target
            } else {
                path
            };
            let meta = fs::metadata(&real).map_err(io(&real))?;
            if meta.is_dir() {
                if !self.ancestors.contains(&real) {
                    self.ancestors.push(real.clone());
                    let result = self.dir(&real, &format!("{prefix}{name}/"));
                    self.ancestors.pop();
                    result?;
                }
            } else if meta.is_file() {
                self.total = self.total.saturating_add(meta.len());
                if self.total > MAX_TOTAL_BYTES {
                    return Err(RelayError::Web(
                        self.root.to_path_buf(),
                        format!("more than {MAX_TOTAL_BYTES} bytes of files"),
                    ));
                }
                let bytes = fs::read(&real).map_err(io(&real))?;
                self.raw.insert(format!("{prefix}{name}"), bytes);
            }
        }
        Ok(())
    }
}

/// The explicit `Content-Type` of a file by extension.
fn content_type(key: &str) -> &'static str {
    let ext = key
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "webmanifest" => "application/manifest+json",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "webp" => "image/webp",
        "jpg" | "jpeg" => "image/jpeg",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "txt" => "text/plain; charset=utf-8",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

/// `%XX`-decodes a request path; `None` for bad escapes or a result that is not UTF-8.
fn percent_decode(s: &str) -> Option<String> {
    if !s.contains('%') {
        return Some(s.to_owned());
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            if !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The `Accept-Encoding` weight of `coding` (an explicit entry beats `*`; unlisted is 0, and a
/// `q` of 0 refuses).
fn q_value(header: Option<&HeaderValue>, coding: &str) -> f32 {
    let Some(value) = header.and_then(|h| h.to_str().ok()) else {
        return 0.0;
    };
    let mut star = 0.0;
    for item in value.split(',') {
        let mut params = item.split(';');
        let name = params.next().unwrap_or("").trim();
        let q = params
            .find_map(|p| {
                let p = p.trim();
                p.strip_prefix("q=").or_else(|| p.strip_prefix("Q="))
            })
            .map_or(1.0, |q| q.trim().parse::<f32>().unwrap_or(0.0))
            .clamp(0.0, 1.0);
        if name.eq_ignore_ascii_case(coding) {
            return q;
        }
        if name == "*" {
            star = q;
        }
    }
    star
}

/// `If-None-Match` (weak comparison): `*` or any listed tag equal to `etag`.
fn etag_matches(inm: &HeaderValue, etag: &HeaderValue) -> bool {
    let (Ok(inm), Ok(etag)) = (inm.to_str(), etag.to_str()) else {
        return false;
    };
    inm.split(',')
        .map(str::trim)
        .any(|tag| tag == "*" || tag.strip_prefix("W/").unwrap_or(tag) == etag)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hv(s: &str) -> HeaderValue {
        HeaderValue::from_str(s).unwrap()
    }

    #[test]
    fn accept_encoding_honours_q_and_star() {
        let q = |h: &str, c: &str| q_value(Some(&hv(h)), c);
        assert!(q("gzip, deflate, br", "br") > 0.0);
        assert_eq!(q("GZIP;q=0.5", "gzip"), 0.5);
        assert_eq!(q("gzip;q=0", "gzip"), 0.0);
        assert_eq!(q("deflate", "gzip"), 0.0);
        assert!(q("*", "br") > 0.0);
        assert_eq!(q("*, br;q=0", "br"), 0.0);
        assert_eq!(q_value(None, "gzip"), 0.0);
    }

    #[test]
    fn higher_q_wins_and_ties_prefer_brotli() {
        let files: &[(&str, &[u8])] = &[
            ("a.css", b"body{}"),
            ("a.css.br", b"BR"),
            ("a.css.gz", b"GZ"),
        ];
        let (_d, web) = app(files, false);
        let enc = |ae: &str| {
            let mut h = HeaderMap::new();
            h.insert(header::ACCEPT_ENCODING, hv(ae));
            let resp = web.respond(&Method::GET, "/a.css", &h);
            resp.headers()
                .get(header::CONTENT_ENCODING)
                .map(|v| v.to_str().unwrap().to_owned())
        };
        assert_eq!(enc("br;q=0.1, gzip;q=1").as_deref(), Some("gzip"));
        assert_eq!(enc("br;q=0, gzip").as_deref(), Some("gzip"));
        assert_eq!(enc("br, gzip").as_deref(), Some("br"));
        assert_eq!(enc("gzip;q=0.5, br;q=0.5").as_deref(), Some("br"));
        assert_eq!(enc("br;q=0, gzip;q=0"), None);
        assert_eq!(enc("*").as_deref(), Some("br"));
    }

    #[test]
    fn if_none_match_lists_and_weak_tags() {
        let tag = hv("\"abc-1\"");
        assert!(etag_matches(&hv("\"abc-1\""), &tag));
        assert!(etag_matches(&hv("\"x\", W/\"abc-1\""), &tag));
        assert!(etag_matches(&hv("*"), &tag));
        assert!(!etag_matches(&hv("\"abc-2\""), &tag));
    }

    #[test]
    fn paths_are_percent_decoded() {
        assert_eq!(percent_decode("a%20b.js").as_deref(), Some("a b.js"));
        assert_eq!(percent_decode("plain").as_deref(), Some("plain"));
        assert_eq!(percent_decode("bad%2"), None);
        assert_eq!(percent_decode("bad%zz"), None);
        assert_eq!(percent_decode("%ff"), None);
        assert_eq!(percent_decode("%+1"), None);
    }

    fn app(files: &[(&str, &[u8])], hsts: bool) -> (tempfile::TempDir, WebApp) {
        let dir = tempfile::tempdir().unwrap();
        for (name, bytes) in files {
            let path = dir.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
        let web = WebApp::load(dir.path(), hsts).unwrap();
        (dir, web)
    }

    #[test]
    fn brotli_wins_over_gzip_and_hsts_follows_tls() {
        let files: &[(&str, &[u8])] = &[
            ("assets/a.css", b"body{}"),
            ("assets/a.css.br", b"BR"),
            ("assets/a.css.gz", b"GZ"),
        ];
        let (_dir, web) = app(files, true);
        assert_eq!(web.file_count(), 1);
        assert_eq!(web.total_bytes(), 10);
        let mut h = HeaderMap::new();
        h.insert(header::ACCEPT_ENCODING, hv("gzip, br"));
        let resp = web.respond(&Method::GET, "/assets/a.css", &h);
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()[header::CONTENT_ENCODING], "br");
        assert_eq!(
            resp.headers()[header::CONTENT_TYPE],
            "text/css; charset=utf-8"
        );
        assert_eq!(
            resp.headers()[header::STRICT_TRANSPORT_SECURITY],
            "max-age=31536000"
        );
        let missing = web.respond(&Method::GET, "/b.css", &HeaderMap::new());
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert!(
            missing
                .headers()
                .contains_key(header::STRICT_TRANSPORT_SECURITY)
        );
    }

    #[test]
    fn more_than_the_size_cap_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let f = fs::File::create(dir.path().join("big.bin")).unwrap();
        // Sparse: the cap is checked on the file size before reading.
        f.set_len(MAX_TOTAL_BYTES + 1).unwrap();
        assert!(matches!(
            WebApp::load(dir.path(), false),
            Err(RelayError::Web(..))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_must_stay_inside_the_web_dir() {
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), b"secret").unwrap();
        let (dir, web) = app(&[("index.html", b"<p>")], false);
        std::os::unix::fs::symlink("index.html", dir.path().join("home.html")).unwrap();
        drop(web);
        let web = WebApp::load(dir.path(), false).unwrap();
        assert_eq!(
            web.respond(&Method::GET, "/home.html", &HeaderMap::new())
                .status(),
            StatusCode::OK
        );
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            dir.path().join("leak.txt"),
        )
        .unwrap();
        assert!(matches!(
            WebApp::load(dir.path(), false),
            Err(RelayError::Web(..))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn directory_symlinks_do_not_shadow_their_targets() {
        // Links named to sort before and after the target, whatever order `read_dir` yields.
        for link in ["0/loop", "zz/loop", "assets/self"] {
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir_all(dir.path().join("assets")).unwrap();
            fs::write(dir.path().join("assets/core.js"), b"js").unwrap();
            fs::write(dir.path().join("index.html"), b"<p>").unwrap();
            let link_path = dir.path().join(link);
            fs::create_dir_all(link_path.parent().unwrap()).unwrap();
            let up = "../".repeat(link.matches('/').count());
            std::os::unix::fs::symlink(format!("{up}assets"), &link_path).unwrap();
            // A link back up the ancestry is a cycle and is skipped, not an error.
            std::os::unix::fs::symlink("..", dir.path().join("assets/up")).unwrap();
            let web = WebApp::load(dir.path(), false).unwrap();
            let get = |p: &str| web.respond(&Method::GET, p, &HeaderMap::new()).status();
            assert_eq!(get("/assets/core.js"), StatusCode::OK, "{link}");
            assert_eq!(get("/index.html"), StatusCode::OK, "{link}");
        }
    }
}

//! C ABI for headless Blitz rendering.
//!
//! This is `examples/screenshot.rs` turned inside out: instead of a `main` that
//! reads argv and writes a PNG next to the manifest, it exposes a handful of
//! `extern "C"` entry points that render HTML to an RGBA buffer and let the
//! caller decide what to do with the pixels.
//!
//! Design constraints that come from being a static library rather than a binary:
//!
//! * Nothing panics across the FFI boundary. Every entry point wraps its body in
//!   `catch_unwind` and converts a panic into `BLITZ_ERR_PANIC`.
//! * No global state is installed on load (no logger, no panic hook, no
//!   `#[tokio::main]`). The tokio runtime lives inside an explicit context handle
//!   whose lifetime the caller controls.
//! * The asset-settling loop is bounded by a timeout instead of spinning
//!   forever. The example's `loop { resolve; if net.is_empty() { break } }` is a
//!   busy-wait that never terminates if a fetch hangs.
//! * All allocations returned to C are freed by matching `blitz_*_free` calls,
//!   never by the host allocator.

// Every entry point here is `unsafe extern "C"` and takes raw pointers, so
// clippy's safety-doc lint fires on all of them. The contracts are stated in
// blitz.h, which is what a caller actually reads; duplicating them as rustdoc
// "# Safety" sections would drift.
#![allow(clippy::missing_safety_doc)]

use std::ffi::{CStr, CString, c_char, c_int};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::ptr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod markdown;
mod net;

use anyrender::{PaintScene as _, render_to_buffer};
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::{DocumentConfig, util::Color};
use blitz_html::HtmlDocument;
use blitz_paint::paint_scene;
use blitz_traits::shell::{ColorScheme, Viewport};
use peniko::Fill;
use peniko::kurbo::Rect;
use url::Url;

const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:60.0) Gecko/20100101 Firefox/81.0";

// ---------------------------------------------------------------------------
// Status codes
// ---------------------------------------------------------------------------

pub const BLITZ_OK: c_int = 0;
pub const BLITZ_ERR_INVALID_ARG: c_int = -1;
pub const BLITZ_ERR_INVALID_UTF8: c_int = -2;
pub const BLITZ_ERR_INVALID_URL: c_int = -3;
pub const BLITZ_ERR_NETWORK: c_int = -4;
pub const BLITZ_ERR_IO: c_int = -5;
pub const BLITZ_ERR_RENDER: c_int = -6;
pub const BLITZ_ERR_PANIC: c_int = -7;

// ---------------------------------------------------------------------------
// Thread-local error channel
// ---------------------------------------------------------------------------

thread_local! {
    static LAST_ERROR: std::cell::RefCell<Option<CString>> =
        const { std::cell::RefCell::new(None) };
}

fn set_error(msg: impl Into<Vec<u8>>) {
    let cleaned: Vec<u8> = msg.into().into_iter().filter(|b| *b != 0).collect();
    let c = CString::new(cleaned).unwrap_or_else(|_| CString::new("unknown error").unwrap());
    LAST_ERROR.with(|slot| *slot.borrow_mut() = Some(c));
}

fn clear_error() {
    LAST_ERROR.with(|slot| *slot.borrow_mut() = None);
}

/// Message for the most recent failure **on the calling thread**.
///
/// Returns NULL when there is no pending error. The pointer is owned by the
/// library and is invalidated by the next `blitz_*` call on this thread, so copy
/// the string if you need to keep it.
#[unsafe(no_mangle)]
pub extern "C" fn blitz_last_error_message() -> *const c_char {
    LAST_ERROR.with(|slot| match slot.borrow().as_ref() {
        Some(c) => c.as_ptr(),
        None => ptr::null(),
    })
}

/// Semver string of the library. Static; never freed.
#[unsafe(no_mangle)]
pub extern "C" fn blitz_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

pub const BLITZ_COLOR_SCHEME_LIGHT: u32 = 0;
pub const BLITZ_COLOR_SCHEME_DARK: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct BlitzRenderOptions {
    /// Viewport width in CSS pixels. 0 -> 1200.
    pub width: u32,
    /// Minimum viewport height in CSS pixels. 0 -> 800.
    pub height: u32,
    /// Device pixel ratio. <= 0.0 -> 1.0.
    pub scale: f32,
    /// Hard cap on the rendered height in CSS pixels, before scale. 0 -> 4000.
    pub max_height: u32,
    /// `BLITZ_COLOR_SCHEME_*`.
    pub color_scheme: u32,
    /// Backdrop painted under the document, as 0xRRGGBBAA. An alpha of 0 leaves
    /// the buffer transparent, which is what you want when compositing.
    pub background_rgba: u32,
    /// How long to keep resolving while sub-resources are still in flight.
    /// 0 -> 10000.
    pub net_timeout_ms: u32,
    /// NUL-terminated override, or NULL for the default Firefox-ish string.
    pub user_agent: *const c_char,
    /// 0 disables all sub-resource fetching (images, stylesheets, fonts).
    pub enable_net: u8,
    /// Grow the render height to fit the document instead of clipping to
    /// `height`. This is the screenshot example's behaviour.
    pub fit_content_height: u8,
    pub _reserved: [u8; 2],
}

impl Default for BlitzRenderOptions {
    fn default() -> Self {
        Self {
            width: 1200,
            height: 800,
            scale: 1.0,
            max_height: 4000,
            color_scheme: BLITZ_COLOR_SCHEME_LIGHT,
            background_rgba: 0xFFFF_FFFF,
            net_timeout_ms: 10_000,
            user_agent: ptr::null(),
            enable_net: 1,
            fit_content_height: 1,
            _reserved: [0; 2],
        }
    }
}

/// Zero-initialised structs are legal but ambiguous; prefer this so new fields
/// pick up sensible values without changing call sites.
#[unsafe(no_mangle)]
pub extern "C" fn blitz_render_options_default() -> BlitzRenderOptions {
    BlitzRenderOptions::default()
}

struct ResolvedOptions {
    css_width: u32,
    css_height: u32,
    scale: f64,
    max_height: u32,
    color_scheme: ColorScheme,
    background: Option<Color>,
    net_timeout: Duration,
    user_agent: String,
    enable_net: bool,
    fit_content_height: bool,
}

impl ResolvedOptions {
    /// # Safety
    /// `opts` must be NULL or point to a valid `BlitzRenderOptions`, whose
    /// `user_agent` is NULL or a valid NUL-terminated string.
    unsafe fn from_raw(opts: *const BlitzRenderOptions) -> Result<Self, (c_int, String)> {
        let raw = if opts.is_null() {
            BlitzRenderOptions::default()
        } else {
            unsafe { *opts }
        };

        let user_agent = if raw.user_agent.is_null() {
            DEFAULT_USER_AGENT.to_owned()
        } else {
            unsafe { CStr::from_ptr(raw.user_agent) }
                .to_str()
                .map_err(|_| {
                    (
                        BLITZ_ERR_INVALID_UTF8,
                        "user_agent is not valid UTF-8".to_owned(),
                    )
                })?
                .to_owned()
        };

        let alpha = (raw.background_rgba & 0xFF) as u8;
        let background = (alpha != 0).then(|| {
            Color::from_rgba8(
                ((raw.background_rgba >> 24) & 0xFF) as u8,
                ((raw.background_rgba >> 16) & 0xFF) as u8,
                ((raw.background_rgba >> 8) & 0xFF) as u8,
                alpha,
            )
        });

        Ok(Self {
            css_width: if raw.width == 0 { 1200 } else { raw.width },
            css_height: if raw.height == 0 { 800 } else { raw.height },
            scale: if raw.scale <= 0.0 {
                1.0
            } else {
                raw.scale as f64
            },
            max_height: if raw.max_height == 0 {
                4000
            } else {
                raw.max_height
            },
            color_scheme: match raw.color_scheme {
                BLITZ_COLOR_SCHEME_DARK => ColorScheme::Dark,
                _ => ColorScheme::Light,
            },
            background,
            net_timeout: Duration::from_millis(if raw.net_timeout_ms == 0 {
                10_000
            } else {
                raw.net_timeout_ms as u64
            }),
            user_agent,
            enable_net: raw.enable_net != 0,
            fit_content_height: raw.fit_content_height != 0,
        })
    }
}

// ---------------------------------------------------------------------------
// Owned buffers
// ---------------------------------------------------------------------------

/// A decoded RGBA8 image. `cap` is bookkeeping for the Rust allocator: read it,
/// don't write it.
#[repr(C)]
pub struct BlitzImage {
    pub data: *mut u8,
    pub len: usize,
    pub cap: usize,
    pub width: u32,
    pub height: u32,
    /// Bytes per row. Always `width * 4` today, but read it rather than assuming.
    pub stride: u32,
}

impl BlitzImage {
    /// A zeroed image, for initialising an out-parameter before a render call.
    pub fn empty() -> Self {
        Self {
            data: ptr::null_mut(),
            len: 0,
            cap: 0,
            width: 0,
            height: 0,
            stride: 0,
        }
    }
}

impl Default for BlitzImage {
    fn default() -> Self {
        Self::empty()
    }
}

/// An owned byte run (encoded PNG, typically).
#[repr(C)]
pub struct BlitzBuffer {
    pub data: *mut u8,
    pub len: usize,
    pub cap: usize,
}

fn vec_into_buffer(v: Vec<u8>) -> BlitzBuffer {
    let mut v = std::mem::ManuallyDrop::new(v);
    BlitzBuffer {
        data: v.as_mut_ptr(),
        len: v.len(),
        cap: v.capacity(),
    }
}

/// Release an image returned by `blitz_render_*`. Idempotent; NULL-safe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn blitz_image_free(image: *mut BlitzImage) {
    if image.is_null() {
        return;
    }
    let img = unsafe { &mut *image };
    if !img.data.is_null() {
        drop(unsafe { Vec::from_raw_parts(img.data, img.len, img.cap) });
    }
    *img = BlitzImage::empty();
}

/// Release a buffer returned by `blitz_image_encode_png`. Idempotent; NULL-safe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn blitz_buffer_free(buf: *mut BlitzBuffer) {
    if buf.is_null() {
        return;
    }
    let b = unsafe { &mut *buf };
    if !b.data.is_null() {
        drop(unsafe { Vec::from_raw_parts(b.data, b.len, b.cap) });
    }
    b.data = ptr::null_mut();
    b.len = 0;
    b.cap = 0;
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

/// Owns the tokio runtime that backs sub-resource fetching.
///
/// Creating one of these is expensive (it spawns worker threads), so hold onto
/// it for the process lifetime rather than making one per render. It is safe to
/// call render functions on it from multiple threads; an internal mutex
/// serialises them, because Stylo keeps process-global style state and the
/// documents themselves are not `Sync`.
pub struct BlitzContext {
    runtime: tokio::runtime::Runtime,
    render_lock: Mutex<()>,
}

/// With `rustls-no-provider`, rustls has no default crypto provider and every
/// handshake fails until one is installed. Do it once per process, on the first
/// context creation, rather than in a constructor the caller might never reach.
#[cfg(feature = "tls-ring")]
fn install_crypto_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Already-installed is not an error: a host process linking this
        // library may have installed its own provider first, and theirs wins.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[cfg(not(feature = "tls-ring"))]
fn install_crypto_provider() {}

/// Create a rendering context. `worker_threads` of 0 lets tokio pick.
/// Returns NULL on failure; call `blitz_last_error_message`.
#[unsafe(no_mangle)]
pub extern "C" fn blitz_context_new(worker_threads: u32) -> *mut BlitzContext {
    clear_error();
    let result = catch_unwind(|| {
        install_crypto_provider();

        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.enable_all().thread_name("blitz-net");
        if worker_threads > 0 {
            builder.worker_threads(worker_threads as usize);
        }
        builder.build()
    });

    match result {
        Ok(Ok(runtime)) => Box::into_raw(Box::new(BlitzContext {
            runtime,
            render_lock: Mutex::new(()),
        })),
        Ok(Err(e)) => {
            set_error(format!("failed to start tokio runtime: {e}"));
            ptr::null_mut()
        }
        Err(_) => {
            set_error("panic while creating context");
            ptr::null_mut()
        }
    }
}

/// Shut down a context and join its worker threads. NULL-safe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn blitz_context_free(ctx: *mut BlitzContext) {
    if ctx.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        drop(unsafe { Box::from_raw(ctx) });
    }));
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render markup that the caller already has in hand.
///
/// `base_url` may be NULL, in which case relative URLs in the document won't
/// resolve. Writes into `out` on success; `out` is untouched on failure.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn blitz_render_html(
    ctx: *mut BlitzContext,
    html: *const c_char,
    base_url: *const c_char,
    opts: *const BlitzRenderOptions,
    out: *mut BlitzImage,
) -> c_int {
    clear_error();
    guard(|| {
        let ctx = require_ctx(ctx)?;
        let out = require_out(out)?;
        let html = cstr(html, "html")?;
        let base = if base_url.is_null() {
            None
        } else {
            Some(cstr(base_url, "base_url")?.to_owned())
        };
        let options = unsafe { ResolvedOptions::from_raw(opts) }?;

        let _lock = ctx.render_lock.lock().unwrap_or_else(|e| e.into_inner());
        let image = render(ctx, html, base, &options)?;
        *out = image;
        Ok(())
    })
}

/// Fetch a URL and render it. Handles `file:` locally and everything else over
/// HTTP. A bare host like `example.com` is upgraded to `https://example.com`,
/// matching the screenshot example's behaviour.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn blitz_render_url(
    ctx: *mut BlitzContext,
    url: *const c_char,
    opts: *const BlitzRenderOptions,
    out: *mut BlitzImage,
) -> c_int {
    clear_error();
    guard(|| {
        let ctx = require_ctx(ctx)?;
        let out = require_out(out)?;
        let url_str = cstr(url, "url")?;
        let options = unsafe { ResolvedOptions::from_raw(opts) }?;

        let parsed = Url::parse(url_str)
            .or_else(|_| Url::parse(&format!("https://{url_str}")))
            .map_err(|e| {
                (
                    BLITZ_ERR_INVALID_URL,
                    format!("invalid url {url_str:?}: {e}"),
                )
            })?;
        let canonical = parsed.to_string();

        let html = fetch(ctx, &parsed, &options)?;

        let _lock = ctx.render_lock.lock().unwrap_or_else(|e| e.into_inner());
        let image = render(ctx, &html, Some(canonical), &options)?;
        *out = image;
        Ok(())
    })
}

/// Render a markdown document, passed as a NUL-terminated UTF-8 string.
///
/// The markdown is converted to HTML and styled, then goes through the same
/// path as `blitz_render_html`. GFM tables, footnotes, strikethrough, task lists
/// and smart punctuation are enabled.
///
/// `stylesheet` selects the CSS:
///   * NULL          - use the built-in stylesheet
///   * ""            - no stylesheet at all (unstyled document)
///   * anything else - use it verbatim, replacing the built-in one
///
/// `base_url` may be NULL. Supply it if the markdown references relative image
/// paths — a `file://` directory URL works for local files, and note that
/// images only load when `enable_net` is set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn blitz_render_markdown(
    ctx: *mut BlitzContext,
    markdown: *const c_char,
    base_url: *const c_char,
    stylesheet: *const c_char,
    opts: *const BlitzRenderOptions,
    out: *mut BlitzImage,
) -> c_int {
    clear_error();
    guard(|| {
        let ctx = require_ctx(ctx)?;
        let out = require_out(out)?;
        let source = cstr(markdown, "markdown")?;
        let base = if base_url.is_null() {
            None
        } else {
            Some(cstr(base_url, "base_url")?.to_owned())
        };
        let css = if stylesheet.is_null() {
            None
        } else {
            Some(cstr(stylesheet, "stylesheet")?)
        };
        let options = unsafe { ResolvedOptions::from_raw(opts) }?;

        let html = markdown::to_html_document(source, css);

        let _lock = ctx.render_lock.lock().unwrap_or_else(|e| e.into_inner());
        let image = render(ctx, &html, base, &options)?;
        *out = image;
        Ok(())
    })
}

/// Convert markdown to a styled HTML document without rendering it.
///
/// Useful for debugging a layout problem, or for handing the HTML to something
/// else. The buffer holds UTF-8 and is *not* NUL-terminated; use `len`. Release
/// it with `blitz_buffer_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn blitz_markdown_to_html(
    markdown: *const c_char,
    stylesheet: *const c_char,
    out: *mut BlitzBuffer,
) -> c_int {
    clear_error();
    guard(|| {
        if out.is_null() {
            return Err((BLITZ_ERR_INVALID_ARG, "out is NULL".to_owned()));
        }
        let source = cstr(markdown, "markdown")?;
        let css = if stylesheet.is_null() {
            None
        } else {
            Some(cstr(stylesheet, "stylesheet")?)
        };
        let html = markdown::to_html_document(source, css);
        unsafe { *out = vec_into_buffer(html.into_bytes()) };
        Ok(())
    })
}

/// The built-in markdown stylesheet, as a static NUL-terminated string. Never
/// freed. Handy as a starting point for a customised sheet.
#[unsafe(no_mangle)]
pub extern "C" fn blitz_default_markdown_stylesheet() -> *const c_char {
    markdown::DEFAULT_STYLESHEET_NUL.as_ptr().cast()
}

fn fetch(
    ctx: &BlitzContext,
    url: &Url,
    options: &ResolvedOptions,
) -> Result<String, (c_int, String)> {
    if url.scheme() == "file" {
        let path = url
            .to_file_path()
            .map_err(|_| (BLITZ_ERR_INVALID_URL, format!("bad file url: {url}")))?;
        let bytes = std::fs::read(&path)
            .map_err(|e| (BLITZ_ERR_IO, format!("read {}: {e}", path.display())))?;
        return String::from_utf8(bytes)
            .map_err(|e| (BLITZ_ERR_INVALID_UTF8, format!("{url} is not UTF-8: {e}")));
    }

    let url = url.clone();
    let user_agent = options.user_agent.clone();
    let timeout = options.net_timeout;

    ctx.runtime.block_on(async move {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| (BLITZ_ERR_NETWORK, format!("http client: {e}")))?;
        let response = client
            .get(url.clone())
            .header("User-Agent", user_agent)
            .send()
            .await
            .map_err(|e| (BLITZ_ERR_NETWORK, format!("GET {url}: {e}")))?;
        response
            .text()
            .await
            .map_err(|e| (BLITZ_ERR_NETWORK, format!("body of {url}: {e}")))
    })
}

fn render(
    ctx: &BlitzContext,
    html: &str,
    base_url: Option<String>,
    options: &ResolvedOptions,
) -> Result<BlitzImage, (c_int, String)> {
    // Document creation and resolution are synchronous, but the net provider
    // spawns tasks, so they need to run inside the runtime's context.
    let _enter = ctx.runtime.enter();

    let net = if options.enable_net {
        Some(Arc::new(net::Provider::new(options)?))
    } else {
        None
    };

    let viewport = Viewport::new(
        (options.css_width as f64 * options.scale) as u32,
        (options.css_height as f64 * options.scale) as u32,
        options.scale as f32,
        options.color_scheme,
    );

    let mut document = HtmlDocument::from_html(
        html,
        DocumentConfig {
            base_url,
            net_provider: net.clone().map(|n| n as _),
            viewport: Some(viewport),
            ..Default::default()
        },
    );

    // Settle sub-resources. The example spins here unconditionally; bound it so
    // a stalled fetch degrades to a partially-loaded render instead of hanging
    // the caller's thread forever.
    let deadline = Instant::now() + options.net_timeout;
    loop {
        document.resolve(0.0);
        match &net {
            Some(n) if n.has_pending() => {}
            _ => break,
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    // Final style + layout pass now that assets have intrinsic sizes.
    document.as_mut().resolve(0.0);

    let content_height = document.as_ref().root_element().final_layout().size.height as f64;
    let css_height = if options.fit_content_height {
        content_height
            .max(options.css_height as f64)
            .min(options.max_height as f64)
    } else {
        options.css_height as f64
    };

    let render_width = ((options.css_width as f64 * options.scale) as u32).max(1);
    let render_height = ((css_height * options.scale) as u32).max(1);

    let background = options.background;
    let buffer = render_to_buffer::<VelloCpuImageRenderer, _>(
        |scene| {
            if let Some(color) = background {
                scene.fill(
                    Fill::NonZero,
                    Default::default(),
                    color,
                    Default::default(),
                    &Rect::new(0.0, 0.0, render_width as f64, render_height as f64),
                );
            }
            paint_scene(
                scene,
                document.as_mut(),
                options.scale,
                render_width,
                render_height,
                0,
                0,
            );
        },
        render_width,
        render_height,
    );

    let expected = render_width as usize * render_height as usize * 4;
    if buffer.len() < expected {
        return Err((
            BLITZ_ERR_RENDER,
            format!(
                "renderer produced {} bytes, expected {expected}",
                buffer.len()
            ),
        ));
    }

    let mut buffer = std::mem::ManuallyDrop::new(buffer);
    Ok(BlitzImage {
        data: buffer.as_mut_ptr(),
        len: buffer.len(),
        cap: buffer.capacity(),
        width: render_width,
        height: render_height,
        stride: render_width * 4,
    })
}

// ---------------------------------------------------------------------------
// PNG encoding
// ---------------------------------------------------------------------------

/// Encode an image as PNG into a freshly allocated buffer.
/// `dpi` of 0 -> 144, matching the screenshot example.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn blitz_image_encode_png(
    image: *const BlitzImage,
    dpi: u32,
    out: *mut BlitzBuffer,
) -> c_int {
    clear_error();
    guard(|| {
        if out.is_null() {
            return Err((BLITZ_ERR_INVALID_ARG, "out is NULL".to_owned()));
        }
        let img = unsafe { require_image(image) }?;
        let mut encoded = Vec::with_capacity(img.len / 4);
        write_png(&mut encoded, img, dpi)?;
        unsafe { *out = vec_into_buffer(encoded) };
        Ok(())
    })
}

/// Encode an image as PNG straight to a filesystem path, skipping the
/// intermediate allocation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn blitz_image_write_png(
    image: *const BlitzImage,
    path: *const c_char,
    dpi: u32,
) -> c_int {
    clear_error();
    guard(|| {
        let img = unsafe { require_image(image) }?;
        let path = cstr(path, "path")?;
        let file = std::fs::File::create(Path::new(path))
            .map_err(|e| (BLITZ_ERR_IO, format!("create {path}: {e}")))?;
        write_png(std::io::BufWriter::new(file), img, dpi)
    })
}

fn write_png<W: std::io::Write>(
    writer: W,
    img: &BlitzImage,
    dpi: u32,
) -> Result<(), (c_int, String)> {
    let dpi = if dpi == 0 { 144.0 } else { dpi as f64 };
    let ppm = (dpi * 39.3701) as u32;

    let pixels = unsafe { std::slice::from_raw_parts(img.data, img.len) };

    let mut encoder = png::Encoder::new(writer, img.width, img.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_pixel_dims(Some(png::PixelDimensions {
        xppu: ppm,
        yppu: ppm,
        unit: png::Unit::Meter,
    }));

    let mut writer = encoder
        .write_header()
        .map_err(|e| (BLITZ_ERR_IO, format!("png header: {e}")))?;
    writer
        .write_image_data(pixels)
        .map_err(|e| (BLITZ_ERR_IO, format!("png data: {e}")))?;
    writer
        .finish()
        .map_err(|e| (BLITZ_ERR_IO, format!("png finish: {e}")))
}

// ---------------------------------------------------------------------------
// Boundary helpers
// ---------------------------------------------------------------------------

/// Runs `f`, converting `Err` into a status code and a thread-local message,
/// and converting a panic into `BLITZ_ERR_PANIC`. Unwinding into C is UB, so
/// every entry point goes through here.
fn guard<F>(f: F) -> c_int
where
    F: FnOnce() -> Result<(), (c_int, String)>,
{
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(())) => BLITZ_OK,
        Ok(Err((code, msg))) => {
            set_error(msg);
            code
        }
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panic in blitz".to_owned());
            set_error(format!("panic: {msg}"));
            BLITZ_ERR_PANIC
        }
    }
}

fn require_ctx(ctx: *mut BlitzContext) -> Result<&'static BlitzContext, (c_int, String)> {
    if ctx.is_null() {
        return Err((BLITZ_ERR_INVALID_ARG, "context is NULL".to_owned()));
    }
    Ok(unsafe { &*ctx })
}

fn require_out(out: *mut BlitzImage) -> Result<&'static mut BlitzImage, (c_int, String)> {
    if out.is_null() {
        return Err((BLITZ_ERR_INVALID_ARG, "out is NULL".to_owned()));
    }
    Ok(unsafe { &mut *out })
}

unsafe fn require_image(image: *const BlitzImage) -> Result<&'static BlitzImage, (c_int, String)> {
    if image.is_null() {
        return Err((BLITZ_ERR_INVALID_ARG, "image is NULL".to_owned()));
    }
    let img = unsafe { &*image };
    if img.data.is_null() || img.len == 0 {
        return Err((BLITZ_ERR_INVALID_ARG, "image is empty".to_owned()));
    }
    Ok(img)
}

fn cstr<'a>(p: *const c_char, name: &str) -> Result<&'a str, (c_int, String)> {
    if p.is_null() {
        return Err((BLITZ_ERR_INVALID_ARG, format!("{name} is NULL")));
    }
    unsafe { CStr::from_ptr(p) }
        .to_str()
        .map_err(|_| (BLITZ_ERR_INVALID_UTF8, format!("{name} is not valid UTF-8")))
}

/// Optional: install a stderr `tracing` subscriber. Only available with the
/// `tracing` feature, and only meaningful once per process.
#[cfg(feature = "tracing")]
#[unsafe(no_mangle)]
pub extern "C" fn blitz_init_tracing() {
    let _ = tracing_subscriber::fmt::try_init();
}

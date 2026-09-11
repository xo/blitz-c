//! Renders real output to `tests/output/` so you can look at it.
//!
//!   cargo test --test render_to_disk -- --include-ignored --nocapture
//!   make screenshots
//!
//! These go through the C entry points rather than the internal Rust functions,
//! so they double as a smoke test of the FFI surface: if a signature or an
//! ownership rule drifts, this fails before any C or Go consumer sees it.
//!
//! `renders_google_to_png` is `#[ignore]`d because it needs the network. A test
//! that fails on a train is a test people learn to ignore, so it's opt-in rather
//! than merely flaky. The markdown test runs everywhere — it disables the net
//! provider entirely.

use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};

use blitz::{
    BLITZ_OK, BlitzContext, BlitzImage, BlitzRenderOptions, blitz_context_free, blitz_context_new,
    blitz_image_free, blitz_image_write_png, blitz_last_error_message, blitz_render_markdown,
    blitz_render_options_default, blitz_render_url,
};

fn output_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/output");
    std::fs::create_dir_all(&dir).expect("create tests/output");
    dir
}

fn last_error() -> String {
    let msg = blitz_last_error_message();
    if msg.is_null() {
        "no detail".to_owned()
    } else {
        unsafe { CStr::from_ptr(msg) }
            .to_string_lossy()
            .into_owned()
    }
}

/// RAII wrapper so a failing assertion still tears down the tokio runtime.
struct Context(*mut BlitzContext);

impl Context {
    fn new() -> Self {
        let ptr = blitz_context_new(0);
        assert!(!ptr.is_null(), "context: {}", last_error());
        Self(ptr)
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        unsafe { blitz_context_free(self.0) };
    }
}

/// Same idea for the image: the buffer is Rust-allocated and must go back
/// through `blitz_image_free`, not be leaked, even on panic.
struct Image(BlitzImage);

impl Drop for Image {
    fn drop(&mut self) {
        unsafe { blitz_image_free(&mut self.0) };
    }
}

fn base_options() -> BlitzRenderOptions {
    let mut opts = blitz_render_options_default();
    opts.width = 1200;
    opts.scale = 2.0;
    opts.height = 800;
    opts
}

/// Write the image out and check it really is a PNG with plausible size.
fn write_and_check(image: &Image, name: &str) -> PathBuf {
    let path = output_dir().join(name);
    let c_path = CString::new(path.to_str().unwrap()).unwrap();

    let rc = unsafe { blitz_image_write_png(&image.0, c_path.as_ptr(), 144) };
    assert_eq!(rc, BLITZ_OK, "write_png: {}", last_error());

    let bytes = std::fs::read(&path).expect("read back the png");
    assert!(
        bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
        "{name} is not a PNG"
    );
    // A blank or trivially small file usually means fonts are missing or the
    // document never laid out, which a status code alone wouldn't catch.
    assert!(
        bytes.len() > 2048,
        "{name} is suspiciously small: {} bytes",
        bytes.len()
    );

    println!(
        "wrote {} ({}x{}, {} KiB)",
        path.display(),
        image.0.width,
        image.0.height,
        bytes.len() / 1024
    );
    path
}

#[test]
fn renders_readme_markdown_to_png() {
    let readme = Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md");
    let source = std::fs::read_to_string(&readme).expect("read README.md");
    let source = CString::new(source).expect("README.md has an interior NUL");

    let ctx = Context::new();

    let mut opts = base_options();
    opts.width = 900;
    // No network: the README's content is self-contained, and this keeps the
    // test hermetic.
    opts.enable_net = 0;

    let mut image = Image(BlitzImage::empty());
    let rc = unsafe {
        blitz_render_markdown(
            ctx.0,
            source.as_ptr(),
            std::ptr::null(), // no base url; nothing relative to resolve
            std::ptr::null(), // built-in stylesheet
            &opts,
            &mut image.0,
        )
    };
    assert_eq!(rc, BLITZ_OK, "render_markdown: {}", last_error());

    assert_eq!(image.0.width, 1800, "900 CSS px at 2x");
    assert!(
        image.0.height > 1000,
        "the README is long; got {}px",
        image.0.height
    );
    assert_eq!(image.0.stride, image.0.width * 4);

    write_and_check(&image, "readme.png");
}

#[test]
#[ignore = "needs network access; run with --include-ignored"]
fn renders_google_to_png() {
    let url = CString::new("https://www.google.com").unwrap();

    let ctx = Context::new();
    let opts = base_options();

    let mut image = Image(BlitzImage::empty());
    let rc = unsafe { blitz_render_url(ctx.0, url.as_ptr(), &opts, &mut image.0) };
    assert_eq!(rc, BLITZ_OK, "render_url: {}", last_error());

    assert_eq!(image.0.width, 2400, "1200 CSS px at 2x");
    assert!(image.0.height >= 1600, "got {}px", image.0.height);

    write_and_check(&image, "google.png");
}

/// Cheap guard against the most annoying FFI regression: a render succeeding but
/// leaving the caller unable to release what it got back.
#[test]
fn freeing_an_image_twice_is_safe() {
    let mut image = BlitzImage::empty();
    unsafe {
        blitz_image_free(&mut image);
        blitz_image_free(&mut image);
    }
    assert!(image.data.is_null());
}

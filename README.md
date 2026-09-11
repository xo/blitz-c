# blitz-c

Headless Blitz rendering behind a C ABI, built as `libblitz.a` for static
linking from C, or from any language with a C FFI. Language bindings live in
their own repos and consume the staged artifacts from `make dist`.

Derived from `examples/screenshot.rs`. Same pipeline — `HtmlDocument` →
resolve → `paint_scene` into `VelloCpuImageRenderer` — with the CLI parsing,
timing printouts and manifest-relative output path replaced by an FFI surface.

## Layout

Standalone repo — nothing needs to be added to the Blitz workspace.

```
Cargo.toml
Makefile
.gitignore
include/blitz.h
src/lib.rs
src/markdown.rs
examples/c/screenshot.c   # renders a URL to a PNG
examples/c/render.c       # renders a URL, .html file, or .md file
.github/workflows/ci.yml  # three-OS matrix
scripts/update-pins.sh    # repin the git dependencies
tests/render_to_disk.rs   # renders real PNGs you can look at
```

Build products (`target/`, `build/`, `dist/`) are gitignored. `Cargo.lock` is
not — with git-pinned dependencies, the lockfile is what makes a given
`libblitz.a` reproducible.

## Quick start

```bash
make run
```

Builds the static library, discovers the system libraries a static link needs,
compiles `examples/c/screenshot.c` against it, and renders
`https://www.google.com` to `google.png` at 1200 CSS px / 2x.

Override any of it:

```bash
make run URL=https://example.com OUT=example.png WIDTH=1400
make run PROFILE=production          # LTO'd, stripped
make native-libs                     # just print the link flags
make dist                            # stage artifacts for other languages
```

The second example takes any of the three input kinds and dispatches on it:

```bash
make render INPUT=README.md                      # -> README.png
make render INPUT=docs/index.html                # -> index.png
make render INPUT=https://example.com            # -> examplecom.png
make render INPUT=README.md OUTPUT=doc.png WIDTH=900
```

It decides by looking at the input: `scheme://` means fetch it as a URL,
a `.md`/`.markdown`/`.mdown`/`.mkd` extension means markdown, and anything else
is treated as HTML. Local files are read directly and given a `file://` base URL
for their containing directory, so relative `<img src>` and stylesheet paths
resolve — that requires `enable_net` to stay on, since assets go through the net
provider even for local documents.

Note that `file:///path/to/x.html` and `./x.html` both work but take different
paths through the library: the first is fetched by `blitz_render_url`, the second
is read by the example and passed to `blitz_render_html`.

The first build is long — Stylo, parley and rustls all compile from source.

## Pinned dependencies

Blitz and AnyRender are both pinned to a revision:

- `dioxuslabs/blitz` @ `b78db85`
- `dioxuslabs/anyrender` @ `562c03e`

The five `blitz-*` crates share one git source and rev, so they resolve to a
single checkout. Splitting them across revs produces two copies of
`blitz-traits` and a wall of trait-mismatch errors.

AnyRender is handled differently, and the difference matters. It's declared
against crates.io in `[dependencies]` and redirected by `[patch.crates-io]`.
A direct git dependency would *not* work: `blitz-paint` depends on `anyrender`
from crates.io, so a git dep here would put two distinct `anyrender` crates in
the graph — `PaintScene` from one isn't `PaintScene` from the other, and
`paint_scene(scene, ...)` stops compiling. Patching rewrites the crates.io
source graph-wide, `blitz-paint` included.

This is also why the crate declares an empty `[workspace]` table: cargo only
honours `[patch]` in a workspace root manifest.

Verify there's exactly one copy before you debug anything else:

```bash
cargo tree -i anyrender
cargo tree -i peniko
cargo tree -d            # lists every duplicated crate
```

If `cargo` reports a patch as unused, or refuses the patch because the git
version doesn't satisfy `0.13.0`, bump the version requirement in
`[dependencies]` to whatever the pinned rev actually declares.

## Build

```bash
cargo build --release
# -> target/release/libblitz.a   (static)
# -> target/release/libblitz.so  (shared, from the cdylib crate-type)
```

For a smaller, faster artifact, use the `production` profile (LTO, one codegen
unit, symbols stripped):

```bash
cargo build --profile production
# -> target/production/libblitz.a
```

## Tests

```bash
make test          # unit tests only; hermetic, no network
make screenshots   # renders tests/output/{readme,google}.png
```

`tests/render_to_disk.rs` drives the C entry points rather than the internal
Rust functions, so it doubles as a smoke test of the FFI surface — a drifting
signature or ownership rule fails here before any downstream consumer sees it.
It checks the PNG magic bytes and a minimum file size, since a status code of
`BLITZ_OK` doesn't tell you whether anything was actually painted; a blank page
usually means no fonts are installed.

The markdown test runs everywhere and disables networking. The `google.com` one
is `#[ignore]`d, because a test that fails on a train is a test people learn to
ignore — `make screenshots` passes `--include-ignored` to run it. Output lands
in `tests/output/`, which is gitignored.

## CI

`.github/workflows/ci.yml` runs four jobs:

- **test** — a matrix over `ubuntu-latest`, `macos-latest` and `windows-latest`.
  Builds the archive, checks for duplicated `anyrender`/`peniko` in the tree,
  prints the native link flags, and runs the hermetic tests.
- **lint** — `cargo fmt --check`, `cargo clippy -D warnings`, the C examples
  rebuilt with `-Werror`, and shellcheck on the update script.
- **network** — renders google.com for real. `continue-on-error: true`, because
  it depends on an external site that can rate-limit or redirect datacenter IPs.
  A red mark there means "look at it", not "the code is broken".
- **pins** — reports whether upstream has moved. Informational.

Two things about the matrix are worth knowing.

The C examples are **skipped on Windows**. `render.c` uses `realpath`,
`strcasecmp` and `clock_gettime`, none of which MSVC provides. The library and
`blitz.h` are portable; Windows still builds `blitz.lib` and runs the Rust tests,
which drive the same FFI entry points. Porting the examples would mean an
MSYS2/mingw toolchain or a pile of `#ifdef`s for no real coverage gain.

Linux and macOS runners install fonts explicitly. The renderer resolves fonts
through the host, and a fontless runner produces blank pages that still return
`BLITZ_OK` — so the workflow asserts a minimum PNG size rather than trusting the
status code. Rendered output is uploaded as an artifact on every run.

Windows additionally installs NASM, which `aws-lc-rs` (rustls' crypto provider)
needs to assemble its primitives.

Expect the lint job to want a `cargo fmt` pass on first run.

## Updating the pinned revisions

```bash
make update-pins                              # both repos, latest default branch
bash scripts/update-pins.sh --check           # exit 1 if out of date (for CI)
bash scripts/update-pins.sh --dry-run
bash scripts/update-pins.sh --branch main
bash scripts/update-pins.sh --blitz a1b2c3d   # pin one repo exactly
```

It resolves commits with `git ls-remote`, not the GitHub API, so it needs no
token and has no rate limit. Every `rev` belonging to each repo is rewritten,
including the commented-out `[patch.crates-io]` lines so they stay usable, and
it runs `cargo fetch` afterwards to confirm the new revisions actually resolve —
restoring the original `Cargo.toml` if they don't.

The thing most likely to break on an update is the `[patch]` version
requirements: if a new AnyRender revision bumps its version past `0.13`, the
patch is rejected until you bump the requirement in `[dependencies]` to match.
The script says so when it happens.

If the exec bit didn't survive (zip extraction, `git archive`, some Windows
checkouts), either run it through `bash` as above or `chmod +x scripts/update-pins.sh`.

## TLS

`reqwest` is pinned with `rustls` + `webpki-roots` and `default-features = false`,
which keeps native-tls/OpenSSL out of the static archive entirely. Two things
follow from that choice:

- Root certificates are compiled in, so the library works in a scratch container
  with no `ca-certificates` installed. If you're behind a proxy with a private
  CA, swap `webpki-roots` for `rustls-native-certs` to use the host store.
- The `rustls` feature pulls in `aws-lc-rs` as the crypto provider, which builds
  C and needs `cmake` plus a working C toolchain (and NASM on Windows). If that
  fails to compile, that's the cause — `rustls-no-provider` plus a provider of
  your choice is the escape hatch.

## Find the transitive native libraries

This matters more than usual. A Rust `staticlib` does **not** bundle the system
libraries it depends on, and Blitz's dependency tree is deep — parley/fontique
for font enumeration, rustls + ring for TLS, tokio for I/O. Ask rustc rather
than guessing:

```bash
make native-libs
# equivalently:
cargo rustc --release --crate-type staticlib -- --print native-static-libs
```

It prints a line like `-lgcc_s -lutil -lrt -lpthread -lm -ldl -lc`. The Makefile
feeds this straight into the C link, so the examples need no manual step.

Consumers outside this repo do need it, and many build systems can't compute it
— cgo's `#cgo LDFLAGS` directives, for one, have to be literal. `make dist`
writes the list to `dist/native-static-libs.flags` so it can be copied or read
by a generator script. Getting it wrong produces undefined-symbol errors at link
time, not at runtime.

## Wire up the Go package

```bash
make go-lib
CGO_ENABLED=1 go build ./...
```

`${SRCDIR}` in the cgo directives resolves relative to the Go source file, so
the `lib/` and `include/` directories live next to `blitz.go` and the package is
`go get`-able as a unit. Committing a 100+ MB `.a` to git is unpleasant, though —
for anything beyond a prototype, prefer a `go:generate` step or a Makefile that
builds the Rust side and copies the artifact in.

## Use it from C

`examples/c/screenshot.c` is the reference:

```c
BlitzContext *ctx = blitz_context_new(0);

BlitzRenderOptions opts = blitz_render_options_default();
opts.width = 1200;
opts.scale = 2.0f;

BlitzImage img = {0};
if (blitz_render_url(ctx, "https://www.google.com", &opts, &img) != BLITZ_OK) {
    fprintf(stderr, "%s\n", blitz_last_error_message());
    return 1;
}
blitz_image_write_png(&img, "google.png", 144);

blitz_image_free(&img);
blitz_context_free(ctx);
```

Swap `blitz_render_url` for `blitz_render_markdown(ctx, md, NULL, NULL, &opts, &img)`
to render markdown instead; nothing else changes.

Note `blitz_image_free`, not `free()`. The buffer came from Rust's allocator and
carries its own capacity, so the host allocator can't release it.

## Consuming from another language

```bash
make dist
```

Produces:

```
dist/lib/libblitz.a
dist/include/blitz.h
dist/native-static-libs.flags
```

That's everything a binding needs: the archive, the declarations, and the system
libraries the archive expects alongside it. Bindings (Go/cgo, Python/ctypes,
Node) live in their own repos and copy these in — typically via a release
artifact or a build step, since a `libblitz.a` is far too large to commit
comfortably.

Three rules any binding has to respect:

- Free with `blitz_image_free` / `blitz_buffer_free`, never the host `free()`.
  The buffers come from Rust's allocator and carry their own capacity.
- Read `blitz_last_error_message` immediately after a non-zero return. It's
  thread-local and only valid until the next `blitz_*` call on that thread.
- Copy pixels out before freeing the image if the host language needs to own
  them.

## Notes and constraints

**One context, many renders.** `blitz_context_new` spins up a tokio runtime with
worker threads. Create it once at process start. Renders are serialised behind an
internal mutex because Stylo keeps process-global style state and `HtmlDocument`
is not `Sync`; for real parallelism, run several processes rather than several
contexts.

**Fonts.** The CPU renderer resolves fonts through the host system. In a scratch
container with no fonts installed, text renders as blank boxes. Install at least
`fontconfig` and a font package (`fonts-dejavu-core` is enough) in any image that
runs this.

**Panics don't cross the boundary.** Every entry point is wrapped in
`catch_unwind` and returns `BLITZ_ERR_PANIC` with a message rather than
unwinding into Go, which would abort the process. This does rely on the build
using `panic = "unwind"`. None of the profiles in `Cargo.toml` set
`panic = "abort"` — if you add one that does, abort turns every recoverable
render error into a dead host process.

**Goroutine stacks.** cgo calls run on an OS thread with a system stack, so the
renderer's recursion depth is fine, but a `blitz_render_*` call blocks that
thread for its full duration. Rendering is CPU-heavy; bound your concurrency
rather than firing one goroutine per URL.

**Cross-compiling** with cgo means a full cross toolchain for both Rust and C.
Building inside the target container is almost always less painful.

## Unverified against a live tree

I don't have the crates or a compiler here, so treat the following as the places
most likely to need a small fix on first build. All of them are lifted from
`screenshot.rs`, but the pinned revs are ahead of what I know:

- `DocumentConfig`'s field set — `base_url` here takes `Option<String>` directly,
  where the example passed `Some(url_string)`.
- `Provider::new(None)` — signature and the `net_provider: Some(Arc::clone(&net) as _)`
  coercion.
- `Color::from_rgba8` — the example only used `Color::WHITE`; confirm the
  constructor name in the pinned `color` 0.3.
- `Provider::is_empty` as the settle condition.
- `pulldown-cmark` 0.13's API — `Options::ENABLE_*` flag names have shifted
  across releases, and `ENABLE_HEADING_ATTRIBUTES` in particular is newer than
  the rest. Drop any flag the compiler doesn't recognise.
- Whether the pinned AnyRender rev still declares `anyrender` 0.13 /
  `anyrender_vello_cpu` 0.17. If not, the `[patch]` is rejected and the version
  requirements in `[dependencies]` need bumping to match.
- `peniko` 0.6 — it has to agree with whatever the pinned Blitz and AnyRender
  revs resolve to. `cargo tree -d` will show it if it doesn't.
- Whether `blitz-dom`'s `default` feature is the right one for a no-window build,
  or whether a narrower feature set drops the winit/wgpu paths entirely. Worth
  checking `blitz-dom`'s manifest in the pinned checkout — a smaller `.a` is a
  direct win here. `blitz-paint`'s default features are worth a look too, since
  they decide whether `anyrender_svg` joins the graph and needs patching.

# Builds libblitz.a and the C example that links against it.
#
#   make              build both C examples
#   make run          render https://www.google.com to google.png
#   make run URL=https://example.com OUT=x.png WIDTH=1400
#   make render INPUT=README.md          url, .html or .md -> README.png
#   make render INPUT=docs/i.html OUTPUT=i.png WIDTH=1400
#   make dist         stage libblitz.a + blitz.h + link flags for consumers
#   make screenshots  render tests/output/{readme,google}.png via cargo test
#   make fmt          cargo fmt --all (CI rejects unformatted code)
#   make check-tls    fail if OpenSSL made it into the dependency graph
#   make update-pins  repin the git dependencies to their latest commits
#   make native-libs  show the system libraries a static link needs
#
# PROFILE selects the cargo profile: release (default), production, p2, debug.

PROFILE ?= release
BUILD   ?= build
DIST    ?= dist

ifeq ($(PROFILE),debug)
  CARGO_FLAGS :=
else ifeq ($(PROFILE),release)
  CARGO_FLAGS := --release
else
  CARGO_FLAGS := --profile $(PROFILE)
endif

TARGET_DIR := target/$(PROFILE)
LIB        := $(TARGET_DIR)/libblitz.a
EXAMPLE_SRCS := $(wildcard examples/c/*.c)
EXAMPLE_BINS := $(patsubst examples/c/%.c,$(BUILD)/%,$(EXAMPLE_SRCS))
NATIVE_LIB_FLAGS := $(BUILD)/native-static-libs.flags
RUST_SRC   := Cargo.toml $(wildcard src/*.rs)

CC      ?= cc
CFLAGS  ?= -std=c11 -O2 -Wall -Wextra
CPPFLAGS += -Iinclude

# `make run` renders a URL; `make render INPUT=...` takes a URL or a local
# .html / .md file.
URL    ?= https://www.google.com
OUT    ?= google.png
WIDTH  ?= 1200
INPUT  ?= README.md
OUTPUT ?=

.PHONY: all lib examples run render native-libs dist test screenshots fmt \
        check-tls update-pins clean distclean

all: examples

lib: $(LIB)

$(LIB): $(RUST_SRC)
	cargo build $(CARGO_FLAGS)
	@touch $@

# A Rust staticlib does not bundle the system libraries it depends on, and this
# dependency tree is deep (rustls/aws-lc, fontique, tokio). Ask rustc for the
# list rather than hardcoding it — the answer differs by platform and by which
# features are enabled.
#
# Colour is forced off on the cargo side, which is what decides whether rustc is
# asked to render diagnostics with ANSI escapes. CI exports
# CARGO_TERM_COLOR=always, and the escapes that adds around the `note:` line
# stop the pattern below from matching anything. Don't pass `--color` to rustc
# instead: it conflicts with the `--error-format=json` cargo already passes, and
# the crate fails to compile.
#
# There is deliberately no hardcoded fallback. The lists this used to carry were
# both wrong (Linux was missing -lfontconfig, macOS -framework Foundation), and
# substituting one silently turned a detection failure into a wall of undefined
# symbols at link time. Failing here names the actual problem.
$(NATIVE_LIB_FLAGS): $(LIB) | $(BUILD)
	@CARGO_TERM_COLOR=never cargo rustc $(CARGO_FLAGS) --color never \
	    --crate-type staticlib -- --print native-static-libs 2>&1 \
	  | sed -n 's/^note: native-static-libs: *//p' | tail -n1 > $@
	@if [ ! -s $@ ]; then \
	    rm -f $@; \
	    echo "error: rustc did not report native-static-libs." >&2; \
	    echo "  reproduce with: cargo rustc $(CARGO_FLAGS) --crate-type staticlib -- --print native-static-libs" >&2; \
	    exit 1; \
	fi

native-libs: $(NATIVE_LIB_FLAGS)
	@cat $<

$(BUILD):
	@mkdir -p $(BUILD)

examples: $(EXAMPLE_BINS)

# Link order matters: object, then the static archive, then the system libs the
# archive refers to.
$(BUILD)/%: examples/c/%.c include/blitz.h $(LIB) $(NATIVE_LIB_FLAGS) | $(BUILD)
	$(CC) $(CFLAGS) $(CPPFLAGS) -o $@ $< \
	    $(LIB) $$(cat $(NATIVE_LIB_FLAGS)) $(LDFLAGS)

run: $(BUILD)/screenshot
	./$(BUILD)/screenshot "$(URL)" "$(OUT)" $(WIDTH)

# OUTPUT is optional — render derives one from the input filename. Width is the
# third positional argument, so it's only passed when OUTPUT is set too.
render: $(BUILD)/render
	./$(BUILD)/render "$(INPUT)" $(if $(OUTPUT),"$(OUTPUT)" $(WIDTH))

fmt:
	cargo fmt --all

# Cargo unions features across the whole graph, so `default-features = false` on
# reqwest does not by itself keep native-tls out: any crate that enables
# reqwest/default-tls drags OpenSSL back in, and it shows up as -lssl -lcrypto.
# `cargo tree -i` exits non-zero when the package isn't in the graph, which is
# exactly the success case here.
check-tls:
	@if cargo tree -i openssl-sys >/dev/null 2>&1; then \
	    echo "OpenSSL is in the dependency graph:"; \
	    cargo tree -e features -i openssl-sys; \
	    exit 1; \
	else \
	    echo "no openssl-sys in the graph"; \
	fi
	@echo "--- rustls copies (must be exactly one) ---"
	@cargo tree -i rustls 2>/dev/null | head -5 || echo "  none"

test:
	cargo test

# Renders tests/output/{readme,google}.png. --include-ignored picks up the
# network-dependent google test, which plain `make test` skips.
screenshots:
	cargo test --test render_to_disk -- --include-ignored --nocapture

# Stage everything a downstream consumer needs: the archive, the header, and the
# system libraries the archive expects to be linked against. Bindings for other
# languages live in their own repos and copy from here.
dist: $(LIB) $(NATIVE_LIB_FLAGS)
	@mkdir -p $(DIST)/lib $(DIST)/include
	cp $(LIB) $(DIST)/lib/
	cp include/blitz.h $(DIST)/include/
	cp $(NATIVE_LIB_FLAGS) $(DIST)/native-static-libs.flags
	@echo
	@echo "staged in $(DIST)/ — link against $(DIST)/lib/libblitz.a with:"
	@cat $(NATIVE_LIB_FLAGS)

# Invoked through bash so a missing exec bit (git archive, zip extraction,
# some Windows checkouts) isn't a failure.
update-pins:
	bash scripts/update-pins.sh $(PINS_ARGS)

clean:
	rm -rf $(BUILD) $(DIST) tests/output

distclean: clean
	cargo clean

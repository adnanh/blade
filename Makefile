PREFIX ?= /usr/local
DESTDIR ?=
CARGO ?= cargo
INSTALL ?= install
RUSTUP ?= rustup
STATIC_TARGET ?= x86_64-unknown-linux-musl

BINDIR := $(DESTDIR)$(PREFIX)/bin
BINARY := target/release/blade
STATIC_BINARY := target/$(STATIC_TARGET)/release/blade

.PHONY: build static install install-static

build:
	$(CARGO) build --release

static:
	@$(RUSTUP) target list --installed | grep -Fxq "$(STATIC_TARGET)" || { \
		echo "Rust target $(STATIC_TARGET) is not installed; run 'rustup target add $(STATIC_TARGET)'" >&2; \
		exit 1; \
	}
	$(CARGO) build --release --target "$(STATIC_TARGET)"
	@echo "Static binary: $(STATIC_BINARY)"

install:
	@test -x "$(BINARY)" || { echo "$(BINARY) is missing; run 'make build' first" >&2; exit 1; }
	$(INSTALL) -Dm755 "$(BINARY)" "$(BINDIR)/blade"

install-static:
	@test -x "$(STATIC_BINARY)" || { echo "$(STATIC_BINARY) is missing; run 'make static' first" >&2; exit 1; }
	$(INSTALL) -Dm755 "$(STATIC_BINARY)" "$(BINDIR)/blade"

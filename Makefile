# gbtoexe — build the converter and the runtime stub it stamps games onto.
#
# The stub is a Windows binary, so building it needs the mingw-w64 linker and
# the Rust target:
#     rustup target add x86_64-pc-windows-gnu
#     (Debian/Ubuntu) apt install gcc-mingw-w64-x86-64
#     (Arch)          pacman -S mingw-w64-gcc
#     (macOS)         brew install mingw-w64

TARGET ?= x86_64-pc-windows-gnu
CARGO  ?= cargo

.PHONY: all stub cli dist test lint clean check-target

all: cli stub

## Fail early, and usefully, if the Windows target is not installed. A machine
## with a distribution Rust package often has no cross target at all, in which
## case rustup's toolchain is the one to build with:
##     make CARGO=~/.cargo/bin/cargo
check-target:
	@$(CARGO) build --release --target $(TARGET) -p gb-payload >/dev/null 2>&1 || { \
	  echo "error: $(CARGO) cannot build for $(TARGET)."; \
	  echo; \
	  echo "  Install the target:   rustup target add $(TARGET)"; \
	  echo "  Install the linker:   mingw-w64-gcc (Arch) / gcc-mingw-w64-x86-64 (Debian)"; \
	  echo "  Using a rustup that is not first on PATH:"; \
	  echo "                        make CARGO=~/.cargo/bin/cargo"; \
	  exit 1; \
	}

## The converter itself, which runs on your machine.
cli:
	$(CARGO) build --release -p gbtoexe

## The runtime every converted game is built on.
stub: check-target
	$(CARGO) build --release --target $(TARGET) -p gb-runtime

## A self-contained directory you can put on PATH or hand to someone else.
dist: all
	mkdir -p dist/stubs
	cp target/release/gbtoexe dist/
	cp target/$(TARGET)/release/gb-runtime.exe dist/stubs/
	@echo
	@echo "dist/ is ready. Convert a game with:"
	@echo "    ./dist/gbtoexe path/to/game.gb"

test:
	$(CARGO) test --release

lint:
	$(CARGO) clippy --release --all-targets -- -D warnings
	$(CARGO) fmt --check

clean:
	$(CARGO) clean
	rm -rf dist

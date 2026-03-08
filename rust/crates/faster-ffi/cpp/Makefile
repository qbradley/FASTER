# Makefile — Build the C++ FASTER wrapper example.
#
# Prerequisites:
#   The Rust FFI crate must be built first:
#     cd <repo>/rust && cargo build --release -p faster-ffi
#
# Usage:
#   make            # Build the example
#   make run        # Build and run
#   make clean      # Remove build artifacts
#   make check      # Compile-only check with both g++ and clang++

# ── Paths ────────────────────────────────────────────────────────────

# Rust workspace root (three levels up from cpp/ → faster-ffi/ → crates/ → rust/)
RUST_ROOT     := $(realpath $(dir $(lastword $(MAKEFILE_LIST)))/../../..)
# Try platform-specific target triple first, fall back to generic
RUST_TARGET   := $(shell rustc -vV | sed -n 's/host: //p')
RELEASE_DIR   := $(RUST_ROOT)/target/$(RUST_TARGET)/release
# Fall back to generic release dir if triple-specific doesn't exist
ifeq ($(wildcard $(RELEASE_DIR)/libfaster_ffi.a),)
  RELEASE_DIR := $(RUST_ROOT)/target/release
endif

FFI_LIB_STATIC := $(RELEASE_DIR)/libfaster_ffi.a
FFI_LIB_SHARED := $(RELEASE_DIR)/libfaster_ffi.so

# ── Compiler settings ────────────────────────────────────────────────

CXX       ?= g++
CXXFLAGS  := -std=c++17 -Wall -Wextra -Wpedantic -O2
LDFLAGS   := -L$(RELEASE_DIR)
LDLIBS    := -lfaster_ffi -lpthread -ldl -lm

# ── Targets ──────────────────────────────────────────────────────────

.PHONY: all run clean check

all: faster_example

faster_example: example.cpp faster_cpp.h $(FFI_LIB_STATIC)
	$(CXX) $(CXXFLAGS) -o $@ example.cpp $(LDFLAGS) $(LDLIBS)

run: faster_example
	LD_LIBRARY_PATH=$(RELEASE_DIR):$$LD_LIBRARY_PATH ./faster_example

clean:
	rm -f faster_example

# Compile-only check with both compilers (no linking).
check: example.cpp faster_cpp.h
	@echo "=== Checking with g++ ==="
	g++ -std=c++17 -Wall -Wextra -Wpedantic -fsyntax-only example.cpp
	@echo "=== Checking with clang++ ==="
	clang++ -std=c++17 -Wall -Wextra -Wpedantic -fsyntax-only example.cpp
	@echo "=== Both compilers OK ==="

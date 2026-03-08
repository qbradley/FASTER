# CMakeLists.txt — Build C++ integration tests for the FASTER Rust FFI wrapper.
#
# Usage:
#   mkdir build && cd build
#   cmake .. -DCMAKE_BUILD_TYPE=Release
#   cmake --build .
#   ctest --output-on-failure
#
# Copyright (c) Microsoft Corporation. Licensed under the MIT License.

cmake_minimum_required(VERSION 3.14)
project(faster_cpp_tests LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# ── Locate the built Rust FFI library ────────────────────────────────

# Detect the Rust target triple
execute_process(
    COMMAND rustc -vV
    OUTPUT_VARIABLE RUSTC_VERSION_OUTPUT
    OUTPUT_STRIP_TRAILING_WHITESPACE
)
string(REGEX MATCH "host: ([^\n]+)" _MATCH "${RUSTC_VERSION_OUTPUT}")
set(RUST_TARGET "${CMAKE_MATCH_1}")

# Rust workspace root: 3 levels up from cpp/ → faster-ffi/ → crates/ → rust/
get_filename_component(RUST_ROOT "${CMAKE_CURRENT_SOURCE_DIR}/../../.." ABSOLUTE)

# Try platform-specific release dir first, then generic
set(RELEASE_DIR "${RUST_ROOT}/target/${RUST_TARGET}/release")
if(NOT EXISTS "${RELEASE_DIR}/libfaster_ffi.a")
    set(RELEASE_DIR "${RUST_ROOT}/target/release")
endif()

message(STATUS "Rust release dir: ${RELEASE_DIR}")

# Import the static library
add_library(faster_ffi STATIC IMPORTED)
set_target_properties(faster_ffi PROPERTIES
    IMPORTED_LOCATION "${RELEASE_DIR}/libfaster_ffi.a"
)

# System libraries required by the Rust cdylib
find_package(Threads REQUIRED)
set(FFI_DEPS Threads::Threads ${CMAKE_DL_LIBS} m)

# ── Example binary ───────────────────────────────────────────────────

add_executable(faster_example example.cpp)
target_link_libraries(faster_example PRIVATE faster_ffi ${FFI_DEPS})

# ── Test binaries ────────────────────────────────────────────────────

enable_testing()

set(TEST_SOURCES
    tests/test_basic_ops.cpp
    tests/test_lifecycle.cpp
    tests/test_threading.cpp
    tests/test_callbacks.cpp
    tests/test_error_handling.cpp
)

foreach(TEST_SRC ${TEST_SOURCES})
    get_filename_component(TEST_NAME ${TEST_SRC} NAME_WE)
    add_executable(${TEST_NAME} ${TEST_SRC})
    target_link_libraries(${TEST_NAME} PRIVATE faster_ffi ${FFI_DEPS})
    target_compile_options(${TEST_NAME} PRIVATE -Wall -Wextra -Wpedantic)
    add_test(NAME ${TEST_NAME} COMMAND ${TEST_NAME})

    # Ensure the shared library can be found at runtime
    set_tests_properties(${TEST_NAME} PROPERTIES
        ENVIRONMENT "LD_LIBRARY_PATH=${RELEASE_DIR}:$ENV{LD_LIBRARY_PATH}"
    )
endforeach()

# ── Convenience targets ──────────────────────────────────────────────

# 'make run_tests' to build and run all tests
add_custom_target(run_tests
    COMMAND ${CMAKE_CTEST_COMMAND} --output-on-failure
    DEPENDS ${TEST_NAMES}
    WORKING_DIRECTORY ${CMAKE_BINARY_DIR}
)

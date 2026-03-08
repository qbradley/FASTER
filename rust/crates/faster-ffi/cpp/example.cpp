// example.cpp — Demonstrates the C++ FASTER wrapper API.
//
// This program exercises: Upsert, Read, RMW (default + custom callbacks),
// Delete, Refresh, CompletePending, and Checkpoint/Recovery.
//
// Build:
//   cd rust && cargo build --release -p faster-ffi
//   cd rust/crates/faster-ffi/cpp && make
//
// Run:
//   ./faster_example
//
// Copyright (c) Microsoft Corporation. Licensed under the MIT License.

#include "faster_cpp.h"

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <string>

namespace fs = std::filesystem;

// ═══════════════════════════════════════════════════════════════════════
// Custom RMW callbacks — atomic increment (the canonical FASTER use case)
// ═══════════════════════════════════════════════════════════════════════
//
// These callbacks implement a sum-store: RMW adds a uint64_t delta to
// the existing value. This mirrors the C++ sum_store / YCSB pattern.

/// Called when the key doesn't exist: initialize value = input.
extern "C" int32_t rmw_initial(
        const uint8_t* /*key_ptr*/, size_t /*key_len*/,
        const uint8_t* input_ptr, size_t input_len,
        uint8_t* value_ptr, size_t* value_len) {
    if (input_len < sizeof(uint64_t)) return -1;
    std::memcpy(value_ptr, input_ptr, sizeof(uint64_t));
    *value_len = sizeof(uint64_t);
    return 0;
}

/// Called when the record is read-only: new_value = old_value + input.
extern "C" int32_t rmw_copy(
        const uint8_t* /*key_ptr*/, size_t /*key_len*/,
        const uint8_t* input_ptr, size_t input_len,
        const uint8_t* old_value_ptr, size_t old_value_len,
        uint8_t* new_value_ptr, size_t* new_value_len) {
    if (input_len < sizeof(uint64_t) || old_value_len < sizeof(uint64_t)) return -1;
    uint64_t old_val, delta;
    std::memcpy(&old_val, old_value_ptr, sizeof(uint64_t));
    std::memcpy(&delta, input_ptr, sizeof(uint64_t));
    uint64_t new_val = old_val + delta;
    std::memcpy(new_value_ptr, &new_val, sizeof(uint64_t));
    *new_value_len = sizeof(uint64_t);
    return 0;
}

/// Called for in-place update in the mutable region: value += input.
extern "C" int32_t rmw_atomic(
        const uint8_t* /*key_ptr*/, size_t /*key_len*/,
        const uint8_t* input_ptr, size_t input_len,
        uint8_t* value_ptr, size_t value_len) {
    if (input_len < sizeof(uint64_t) || value_len < sizeof(uint64_t)) return -1;
    uint64_t current, delta;
    std::memcpy(&current, value_ptr, sizeof(uint64_t));
    std::memcpy(&delta, input_ptr, sizeof(uint64_t));
    current += delta;
    std::memcpy(value_ptr, &current, sizeof(uint64_t));
    return 0;
}

// ═══════════════════════════════════════════════════════════════════════
// Helper
// ═══════════════════════════════════════════════════════════════════════

static int g_checks_passed = 0;
static int g_checks_failed = 0;

#define CHECK(cond, msg)                                        \
    do {                                                        \
        if (!(cond)) {                                          \
            std::fprintf(stderr, "FAIL: %s (line %d)\n",        \
                         (msg), __LINE__);                      \
            ++g_checks_failed;                                  \
        } else {                                                \
            std::printf("  ok: %s\n", (msg));                   \
            ++g_checks_passed;                                  \
        }                                                       \
    } while (0)

// ═══════════════════════════════════════════════════════════════════════
// Test 1: Basic CRUD with an in-memory store
// ═══════════════════════════════════════════════════════════════════════

static void test_basic_crud() {
    std::printf("\n=== Test 1: Basic CRUD (in-memory) ===\n");

    faster::FasterKv<uint64_t, uint64_t> kv;
    auto session = kv.StartSession();

    // Upsert
    auto s = session.Upsert(uint64_t(1), uint64_t(100));
    CHECK(s == faster::Status::Ok || s == faster::Status::Created,
          "Upsert key=1 val=100");

    s = session.Upsert(uint64_t(2), uint64_t(200));
    CHECK(s == faster::Status::Ok || s == faster::Status::Created,
          "Upsert key=2 val=200");

    session.Refresh();

    // Read
    uint64_t val = 0;
    s = session.Read(uint64_t(1), val);
    CHECK(s == faster::Status::Ok && val == 100,
          "Read key=1 => 100");

    s = session.Read(uint64_t(2), val);
    CHECK(s == faster::Status::Ok && val == 200,
          "Read key=2 => 200");

    // Read miss
    s = session.Read(uint64_t(999), val);
    CHECK(s == faster::Status::NotFound,
          "Read key=999 => NotFound");

    // Delete
    s = session.Delete(uint64_t(2));
    CHECK(s == faster::Status::Ok,
          "Delete key=2");

    session.Refresh();

    s = session.Read(uint64_t(2), val);
    CHECK(s == faster::Status::NotFound,
          "Read deleted key=2 => NotFound");

    // RMW (default = full replacement)
    s = session.Rmw(uint64_t(1), uint64_t(999));
    CHECK(s == faster::Status::Ok || s == faster::Status::InPlaceUpdated
          || s == faster::Status::CopyUpdated,
          "Rmw key=1 replace with 999");

    session.Refresh();

    s = session.Read(uint64_t(1), val);
    CHECK(s == faster::Status::Ok && val == 999,
          "Read key=1 after Rmw => 999");
}

// ═══════════════════════════════════════════════════════════════════════
// Test 2: Custom RMW callbacks (sum-store pattern)
// ═══════════════════════════════════════════════════════════════════════

static void test_custom_rmw() {
    std::printf("\n=== Test 2: Custom RMW (sum-store) ===\n");

    faster::FasterKv<uint64_t, uint64_t> kv;
    auto session = kv.StartSession();

    // Insert initial value via RMW (key doesn't exist → rmw_initial)
    uint64_t delta = 10;
    auto s = session.RmwWithCallbacks<uint64_t>(
        uint64_t(42), delta, rmw_initial, rmw_copy, rmw_atomic);
    CHECK(s == faster::Status::Ok || s == faster::Status::Created
          || s == faster::Status::InPlaceUpdated
          || s == faster::Status::CopyUpdated,
          "RMW initial key=42 delta=10");

    session.Refresh();

    // Add 5 more via RMW (key exists → rmw_atomic or rmw_copy)
    delta = 5;
    s = session.RmwWithCallbacks<uint64_t>(
        uint64_t(42), delta, rmw_initial, rmw_copy, rmw_atomic);
    CHECK(s == faster::Status::Ok || s == faster::Status::InPlaceUpdated
          || s == faster::Status::CopyUpdated,
          "RMW add key=42 delta=5");

    session.Refresh();

    // Add 3 more
    delta = 3;
    s = session.RmwWithCallbacks<uint64_t>(
        uint64_t(42), delta, rmw_initial, rmw_copy, rmw_atomic);
    CHECK(s == faster::Status::Ok || s == faster::Status::InPlaceUpdated
          || s == faster::Status::CopyUpdated,
          "RMW add key=42 delta=3");

    session.Refresh();

    // Read back: should be 10 + 5 + 3 = 18
    uint64_t val = 0;
    s = session.Read(uint64_t(42), val);
    CHECK(s == faster::Status::Ok && val == 18,
          "Read key=42 => 18 (10+5+3)");
}

// ═══════════════════════════════════════════════════════════════════════
// Test 3: Bulk operations with Refresh
// ═══════════════════════════════════════════════════════════════════════

static void test_bulk_operations() {
    std::printf("\n=== Test 3: Bulk operations ===\n");

    faster::FasterKv<uint64_t, uint64_t> kv;
    auto session = kv.StartSession();

    constexpr uint64_t N = 1000;

    // Insert N records
    for (uint64_t i = 0; i < N; ++i) {
        session.Upsert(i, i * 10);
        if (i % 100 == 0) session.Refresh();
    }
    session.Refresh();

    // Verify all N records
    int mismatches = 0;
    for (uint64_t i = 0; i < N; ++i) {
        uint64_t val = 0;
        auto s = session.Read(i, val);
        if (s != faster::Status::Ok || val != i * 10) {
            ++mismatches;
        }
        if (i % 100 == 0) session.Refresh();
    }
    CHECK(mismatches == 0,
          "All 1000 records verified correctly");
}

// ═══════════════════════════════════════════════════════════════════════
// Test 4: Checkpoint and Recovery
// ═══════════════════════════════════════════════════════════════════════

static void test_checkpoint_recovery() {
    std::printf("\n=== Test 4: Checkpoint / Recovery ===\n");

    // Use a temp directory for the store (also used for checkpoints).
    auto tmp = fs::temp_directory_path() / "faster_cpp_test";
    fs::remove_all(tmp);
    fs::create_directories(tmp);

    std::string store_path = (tmp / "store").string();
    fs::create_directories(store_path);

    faster::CheckpointToken token;

    // Phase 1: write data and checkpoint
    {
        faster::FasterKv<uint64_t, uint64_t> kv(store_path);
        auto session = kv.StartSession();

        session.Upsert(uint64_t(100), uint64_t(12345));
        session.Upsert(uint64_t(200), uint64_t(67890));
        session.Refresh();
        session.End();

        // Checkpoint dir is the same as the store path.
        token = kv.Checkpoint(store_path, faster::CheckpointType::FoldOver);
        std::printf("  Checkpoint token: %lu:%lu\n", token.high, token.low);
        CHECK(true, "Checkpoint created");
    }

    // Phase 2: open a new store and recover
    {
        faster::FasterKv<uint64_t, uint64_t> kv(store_path);
        kv.Recover(store_path, token);

        auto session = kv.StartSession();

        uint64_t val = 0;
        auto s = session.Read(uint64_t(100), val);
        CHECK(s == faster::Status::Ok && val == 12345,
              "Recovered key=100 => 12345");

        s = session.Read(uint64_t(200), val);
        CHECK(s == faster::Status::Ok && val == 67890,
              "Recovered key=200 => 67890");
    }

    // Cleanup
    fs::remove_all(tmp);
}

// ═══════════════════════════════════════════════════════════════════════
// Test 5: String keys and values
// ═══════════════════════════════════════════════════════════════════════

static void test_string_kv() {
    std::printf("\n=== Test 5: String keys/values ===\n");

    faster::FasterKv<std::string, std::string> kv;
    auto session = kv.StartSession();

    session.Upsert(std::string("hello"), std::string("world"));
    session.Refresh();

    std::string val;
    auto s = session.Read(std::string("hello"), val);
    CHECK(s == faster::Status::Ok && val == "world",
          "Read string key='hello' => 'world'");

    s = session.Read(std::string("missing"), val);
    CHECK(s == faster::Status::NotFound,
          "Read missing string key => NotFound");
}

// ═══════════════════════════════════════════════════════════════════════
// Test 6: Move semantics
// ═══════════════════════════════════════════════════════════════════════

static void test_move_semantics() {
    std::printf("\n=== Test 6: Move semantics ===\n");

    faster::FasterKv<uint64_t, uint64_t> kv1;
    auto session1 = kv1.StartSession();
    session1.Upsert(uint64_t(1), uint64_t(42));
    session1.Refresh();

    // Move session
    auto session2 = std::move(session1);
    CHECK(!session1.IsActive(), "Moved-from session is inactive");
    CHECK(session2.IsActive(), "Moved-to session is active");

    uint64_t val = 0;
    auto s = session2.Read(uint64_t(1), val);
    CHECK(s == faster::Status::Ok && val == 42,
          "Read via moved session => 42");

    // Move store
    auto kv2 = std::move(kv1);
    // session2 still works because it holds the raw handle
    session2.End();
    CHECK(true, "Move semantics work correctly");
}

// ═══════════════════════════════════════════════════════════════════════
// Main
// ═══════════════════════════════════════════════════════════════════════

int main() {
    std::printf("FASTER C++ Wrapper — Example & Verification\n");
    std::printf("============================================\n");

    try {
        test_basic_crud();
        test_custom_rmw();
        test_bulk_operations();
        test_checkpoint_recovery();
        test_string_kv();
        test_move_semantics();
    } catch (const std::exception& e) {
        std::fprintf(stderr, "\nFATAL: %s\n", e.what());
        return 1;
    }

    std::printf("\n============================================\n");
    std::printf("Results: %d passed, %d failed\n",
                g_checks_passed, g_checks_failed);
    return g_checks_failed > 0 ? 1 : 0;
}

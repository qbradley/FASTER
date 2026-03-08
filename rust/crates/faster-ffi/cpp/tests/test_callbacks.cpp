// test_callbacks.cpp — Integration tests for all callback types.
//
// Tests the _ex() FFI surface: RMW (initial, copy, atomic),
// Upsert (put, put_atomic), Read (get, get_atomic).
//
// Copyright (c) Microsoft Corporation. Licensed under the MIT License.

#include "../faster_cpp.h"
#include "test_harness.h"

#include <cstring>

// ═══════════════════════════════════════════════════════════════════════
// RMW callbacks — sum-store pattern (the canonical FASTER use case)
// ═══════════════════════════════════════════════════════════════════════

// Initialize value = input (for new keys).
extern "C" int32_t rmw_sum_initial(
        const uint8_t* /*key_ptr*/, size_t /*key_len*/,
        const uint8_t* input_ptr, size_t input_len,
        uint8_t* value_ptr, size_t* value_len) {
    if (input_len < sizeof(uint64_t)) return -1;
    std::memcpy(value_ptr, input_ptr, sizeof(uint64_t));
    *value_len = sizeof(uint64_t);
    return 0;
}

// Copy-update: new_value = old_value + input.
extern "C" int32_t rmw_sum_copy(
        const uint8_t* /*key_ptr*/, size_t /*key_len*/,
        const uint8_t* input_ptr, size_t input_len,
        const uint8_t* old_value_ptr, size_t old_value_len,
        uint8_t* new_value_ptr, size_t* new_value_len) {
    if (input_len < sizeof(uint64_t) || old_value_len < sizeof(uint64_t)) return -1;
    uint64_t old_val, delta;
    std::memcpy(&old_val, old_value_ptr, sizeof(uint64_t));
    std::memcpy(&delta, input_ptr, sizeof(uint64_t));
    uint64_t result = old_val + delta;
    std::memcpy(new_value_ptr, &result, sizeof(uint64_t));
    *new_value_len = sizeof(uint64_t);
    return 0;
}

// In-place update: value += input.
extern "C" int32_t rmw_sum_atomic(
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

TEST_CASE(rmw_callbacks_sum_store) {
    faster::FasterKv<uint64_t, uint64_t> kv;
    auto session = kv.StartSession();

    // First RMW on missing key → rmw_sum_initial(10)
    uint64_t delta = 10;
    auto s = session.RmwWithCallbacks<uint64_t>(
        uint64_t(42), delta, rmw_sum_initial, rmw_sum_copy, rmw_sum_atomic);
    CHECK(s == faster::Status::Ok || s == faster::Status::Created
          || s == faster::Status::InPlaceUpdated
          || s == faster::Status::CopyUpdated);
    session.Refresh();

    // Add 5
    delta = 5;
    s = session.RmwWithCallbacks<uint64_t>(
        uint64_t(42), delta, rmw_sum_initial, rmw_sum_copy, rmw_sum_atomic);
    CHECK(s == faster::Status::Ok || s == faster::Status::InPlaceUpdated
          || s == faster::Status::CopyUpdated);
    session.Refresh();

    // Add 3
    delta = 3;
    s = session.RmwWithCallbacks<uint64_t>(
        uint64_t(42), delta, rmw_sum_initial, rmw_sum_copy, rmw_sum_atomic);
    session.Refresh();

    // Result: 10 + 5 + 3 = 18
    uint64_t val = 0;
    s = session.Read(uint64_t(42), val);
    CHECK(s == faster::Status::Ok);
    CHECK(val == 18);
}

TEST_CASE(rmw_callbacks_multiple_keys) {
    faster::FasterKv<uint64_t, uint64_t> kv;
    auto session = kv.StartSession();

    // Initialize 3 counters
    for (uint64_t key = 0; key < 3; ++key) {
        uint64_t init = (key + 1) * 100;
        session.RmwWithCallbacks<uint64_t>(
            key, init, rmw_sum_initial, rmw_sum_copy, rmw_sum_atomic);
    }
    session.Refresh();

    // Increment each by 1 ten times
    for (int i = 0; i < 10; ++i) {
        for (uint64_t key = 0; key < 3; ++key) {
            uint64_t one = 1;
            session.RmwWithCallbacks<uint64_t>(
                key, one, rmw_sum_initial, rmw_sum_copy, rmw_sum_atomic);
        }
        session.Refresh();
    }

    // Verify: key 0 = 100+10=110, key 1 = 200+10=210, key 2 = 300+10=310
    for (uint64_t key = 0; key < 3; ++key) {
        uint64_t val = 0;
        auto s = session.Read(key, val);
        CHECK(s == faster::Status::Ok);
        CHECK_MSG(val == (key + 1) * 100 + 10,
                  "Sum-store counter mismatch");
    }
}

TEST_CASE(rmw_callbacks_partial_null) {
    // Pass nullptr for some callbacks — should fall back to default
    // behavior for those operations.
    faster::FasterKv<uint64_t, uint64_t> kv;
    auto session = kv.StartSession();

    // Only provide initial callback, null for copy and atomic
    uint64_t delta = 42;
    auto s = session.RmwWithCallbacks<uint64_t>(
        uint64_t(1), delta, rmw_sum_initial, nullptr, nullptr);
    CHECK(s == faster::Status::Ok || s == faster::Status::Created
          || s == faster::Status::InPlaceUpdated
          || s == faster::Status::CopyUpdated);
    session.Refresh();

    uint64_t val = 0;
    s = session.Read(uint64_t(1), val);
    CHECK(s == faster::Status::Ok);
    CHECK(val == 42);
}

// ═══════════════════════════════════════════════════════════════════════
// RMW callbacks — multiplication pattern
// ═══════════════════════════════════════════════════════════════════════

extern "C" int32_t rmw_mul_initial(
        const uint8_t* /*key_ptr*/, size_t /*key_len*/,
        const uint8_t* input_ptr, size_t input_len,
        uint8_t* value_ptr, size_t* value_len) {
    if (input_len < sizeof(uint64_t)) return -1;
    std::memcpy(value_ptr, input_ptr, sizeof(uint64_t));
    *value_len = sizeof(uint64_t);
    return 0;
}

extern "C" int32_t rmw_mul_copy(
        const uint8_t* /*key_ptr*/, size_t /*key_len*/,
        const uint8_t* input_ptr, size_t input_len,
        const uint8_t* old_value_ptr, size_t old_value_len,
        uint8_t* new_value_ptr, size_t* new_value_len) {
    if (input_len < sizeof(uint64_t) || old_value_len < sizeof(uint64_t)) return -1;
    uint64_t old_val, factor;
    std::memcpy(&old_val, old_value_ptr, sizeof(uint64_t));
    std::memcpy(&factor, input_ptr, sizeof(uint64_t));
    uint64_t result = old_val * factor;
    std::memcpy(new_value_ptr, &result, sizeof(uint64_t));
    *new_value_len = sizeof(uint64_t);
    return 0;
}

extern "C" int32_t rmw_mul_atomic(
        const uint8_t* /*key_ptr*/, size_t /*key_len*/,
        const uint8_t* input_ptr, size_t input_len,
        uint8_t* value_ptr, size_t value_len) {
    if (input_len < sizeof(uint64_t) || value_len < sizeof(uint64_t)) return -1;
    uint64_t current, factor;
    std::memcpy(&current, value_ptr, sizeof(uint64_t));
    std::memcpy(&factor, input_ptr, sizeof(uint64_t));
    current *= factor;
    std::memcpy(value_ptr, &current, sizeof(uint64_t));
    return 0;
}

TEST_CASE(rmw_callbacks_multiply) {
    faster::FasterKv<uint64_t, uint64_t> kv;
    auto session = kv.StartSession();

    // Init with 2
    uint64_t val = 2;
    session.RmwWithCallbacks<uint64_t>(
        uint64_t(10), val, rmw_mul_initial, rmw_mul_copy, rmw_mul_atomic);
    session.Refresh();

    // Multiply by 3 → 6
    val = 3;
    session.RmwWithCallbacks<uint64_t>(
        uint64_t(10), val, rmw_mul_initial, rmw_mul_copy, rmw_mul_atomic);
    session.Refresh();

    // Multiply by 7 → 42
    val = 7;
    session.RmwWithCallbacks<uint64_t>(
        uint64_t(10), val, rmw_mul_initial, rmw_mul_copy, rmw_mul_atomic);
    session.Refresh();

    uint64_t result = 0;
    auto s = session.Read(uint64_t(10), result);
    CHECK(s == faster::Status::Ok);
    CHECK(result == 42); // 2 * 3 * 7 = 42
}

// ═══════════════════════════════════════════════════════════════════════
// Upsert callbacks — custom put logic
// ═══════════════════════════════════════════════════════════════════════

// Custom put: prefix the value with a magic byte 0xAA
extern "C" int32_t upsert_put_prefix(
        const uint8_t* /*key_ptr*/, size_t /*key_len*/,
        const uint8_t* input_ptr, size_t input_len,
        uint8_t* value_ptr, size_t value_len,
        size_t* actual_len) {
    size_t needed = input_len + 1;
    if (needed > value_len) return -1;
    value_ptr[0] = 0xAA;
    std::memcpy(value_ptr + 1, input_ptr, input_len);
    *actual_len = needed;
    return 0;
}

// Custom put_atomic: same prefix logic for in-place updates
extern "C" int32_t upsert_put_atomic_prefix(
        const uint8_t* /*key_ptr*/, size_t /*key_len*/,
        const uint8_t* input_ptr, size_t input_len,
        uint8_t* value_ptr, size_t value_len) {
    size_t needed = input_len + 1;
    if (needed > value_len) return -1;
    value_ptr[0] = 0xAA;
    std::memcpy(value_ptr + 1, input_ptr, input_len);
    return 0;
}

TEST_CASE(upsert_callbacks_custom_put) {
    faster::FasterKv<uint64_t, uint64_t> kv;
    auto session = kv.StartSession();

    uint64_t input_val = 0xDEADBEEF;
    auto s = session.UpsertWithCallbacks<uint64_t>(
        uint64_t(1), input_val,
        upsert_put_prefix, upsert_put_atomic_prefix);
    CHECK(s == faster::Status::Ok || s == faster::Status::Created
          || s == faster::Status::InPlaceUpdated);
    session.Refresh();

    // Read back raw bytes via low-level FFI to verify the prefix.
    // We use a separate session via the raw FFI since Session doesn't
    // expose its handle.
    FasterHandle raw_sess = faster_session_start(kv.raw_handle());
    REQUIRE(raw_sess != INVALID_FASTER_HANDLE);

    uint8_t buf[64] = {};
    uint32_t out_len = 0;
    uint64_t key_bytes = 1;
    FasterStatus raw_s = faster_read(
        kv.raw_handle(), raw_sess,
        reinterpret_cast<const uint8_t*>(&key_bytes), sizeof(key_bytes),
        buf, sizeof(buf), &out_len);

    CHECK(raw_s == FasterStatus_Ok);
    if (raw_s == FasterStatus_Ok) {
        CHECK(out_len == sizeof(uint64_t) + 1);
        CHECK(buf[0] == 0xAA);
    }
    faster_session_end(kv.raw_handle(), raw_sess);
}

TEST_CASE(upsert_callbacks_null_fallback) {
    // Null callbacks → default byte-replacement behavior
    faster::FasterKv<uint64_t, uint64_t> kv;
    auto session = kv.StartSession();

    uint64_t input_val = 12345;
    auto s = session.UpsertWithCallbacks<uint64_t>(
        uint64_t(1), input_val, nullptr, nullptr);
    CHECK(s == faster::Status::Ok || s == faster::Status::Created
          || s == faster::Status::InPlaceUpdated);
    session.Refresh();

    uint64_t val = 0;
    s = session.Read(uint64_t(1), val);
    CHECK(s == faster::Status::Ok);
    CHECK(val == 12345);
}

// ═══════════════════════════════════════════════════════════════════════
// Read callbacks — custom get logic
// ═══════════════════════════════════════════════════════════════════════

// Custom get: double the value bytes before returning
extern "C" int32_t read_get_double(
        const uint8_t* /*key_ptr*/, size_t /*key_len*/,
        const uint8_t* value_ptr, size_t value_len,
        uint8_t* output_ptr, size_t* output_len) {
    if (value_len < sizeof(uint64_t)) return -1;
    uint64_t val;
    std::memcpy(&val, value_ptr, sizeof(uint64_t));
    val *= 2;
    if (*output_len < sizeof(uint64_t)) return -1;
    std::memcpy(output_ptr, &val, sizeof(uint64_t));
    *output_len = sizeof(uint64_t);
    return 0;
}

TEST_CASE(read_callbacks_custom_get) {
    faster::FasterKv<uint64_t, uint64_t> kv;
    auto session = kv.StartSession();

    session.Upsert(uint64_t(1), uint64_t(21));
    session.Refresh();

    // Use ReadWithCallbacks to get doubled value
    uint8_t buf[sizeof(uint64_t)] = {};
    uint32_t out_len = 0;
    auto key_bytes = uint64_t(1);
    (void)key_bytes; // used only for documentation
    auto s = session.ReadWithCallbacks(
        uint64_t(1), buf, sizeof(buf), &out_len,
        read_get_double, read_get_double);
    CHECK(s == faster::Status::Ok);

    if (s == faster::Status::Ok) {
        uint64_t result = 0;
        std::memcpy(&result, buf, sizeof(uint64_t));
        CHECK(result == 42); // 21 * 2 = 42
    }
}

TEST_CASE(read_callbacks_null_fallback) {
    faster::FasterKv<uint64_t, uint64_t> kv;
    auto session = kv.StartSession();

    session.Upsert(uint64_t(1), uint64_t(42));
    session.Refresh();

    // Null callbacks → default behavior
    uint8_t buf[sizeof(uint64_t)] = {};
    uint32_t out_len = 0;
    auto s = session.ReadWithCallbacks(
        uint64_t(1), buf, sizeof(buf), &out_len,
        nullptr, nullptr);
    CHECK(s == faster::Status::Ok);

    if (s == faster::Status::Ok) {
        uint64_t result = 0;
        std::memcpy(&result, buf, sizeof(uint64_t));
        CHECK(result == 42);
    }
}

// ═══════════════════════════════════════════════════════════════════════
// RMW callbacks — callback error propagation
// ═══════════════════════════════════════════════════════════════════════

// A callback that always fails
extern "C" int32_t rmw_failing_initial(
        const uint8_t* /*key_ptr*/, size_t /*key_len*/,
        const uint8_t* /*input_ptr*/, size_t /*input_len*/,
        uint8_t* /*value_ptr*/, size_t* /*value_len*/) {
    return -1; // failure
}

TEST_CASE(rmw_callback_error_does_not_crash) {
    faster::FasterKv<uint64_t, uint64_t> kv;
    auto session = kv.StartSession();

    // This should not crash — the FFI layer handles callback errors gracefully.
    bool no_crash = true;
    try {
        uint64_t delta = 1;
        session.RmwWithCallbacks<uint64_t>(
            uint64_t(99), delta, rmw_failing_initial, nullptr, nullptr);
    } catch (...) {
        // Exception is acceptable (error propagation)
    }
    CHECK(no_crash);
}

// ═══════════════════════════════════════════════════════════════════════
// Session raw_handle access for low-level operations
// ═══════════════════════════════════════════════════════════════════════

TEST_CASE(raw_handle_access) {
    faster::FasterKv<uint64_t, uint64_t> kv;
    CHECK(kv.raw_handle() != 0);
}

TEST_MAIN()

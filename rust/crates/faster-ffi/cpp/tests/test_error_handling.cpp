// test_error_handling.cpp — Integration tests for error paths.
//
// Tests: invalid parameters, double-free prevention, null pointers,
// invalid handles, and exception safety.
//
// Copyright (c) Microsoft Corporation. Licensed under the MIT License.

#include "../faster_cpp.h"
#include "test_harness.h"

// ═══════════════════════════════════════════════════════════════════════
// Invalid handle operations (raw FFI level)
// ═══════════════════════════════════════════════════════════════════════

TEST_CASE(close_invalid_handle) {
    FasterStatus s = faster_close(INVALID_FASTER_HANDLE);
    CHECK(s == FasterStatus_InvalidHandle);
}

TEST_CASE(close_bogus_handle) {
    FasterStatus s = faster_close(9999);
    CHECK(s == FasterStatus_InvalidHandle);
}

TEST_CASE(double_close) {
    FasterHandle h = faster_open();
    REQUIRE(h != INVALID_FASTER_HANDLE);
    CHECK(faster_close(h) == FasterStatus_Ok);
    CHECK(faster_close(h) == FasterStatus_InvalidHandle);
}

TEST_CASE(session_start_invalid_store) {
    FasterHandle s = faster_session_start(INVALID_FASTER_HANDLE);
    CHECK(s == INVALID_FASTER_HANDLE);
}

TEST_CASE(session_start_bogus_store) {
    FasterHandle s = faster_session_start(9999);
    CHECK(s == INVALID_FASTER_HANDLE);
}

TEST_CASE(session_end_invalid_handles) {
    FasterStatus s = faster_session_end(INVALID_FASTER_HANDLE, INVALID_FASTER_HANDLE);
    CHECK(s == FasterStatus_InvalidHandle);
}

// ═══════════════════════════════════════════════════════════════════════
// Null pointer arguments
// ═══════════════════════════════════════════════════════════════════════

TEST_CASE(upsert_null_key_ptr) {
    FasterHandle store = faster_open();
    REQUIRE(store != INVALID_FASTER_HANDLE);
    FasterHandle sess = faster_session_start(store);
    REQUIRE(sess != INVALID_FASTER_HANDLE);

    // Non-zero key_len with null key_ptr → InvalidArgument
    uint8_t val = 1;
    FasterStatus s = faster_upsert(store, sess, nullptr, 10, &val, 1);
    CHECK(s == FasterStatus_InvalidArgument);

    faster_session_end(store, sess);
    faster_close(store);
}

TEST_CASE(upsert_null_val_ptr) {
    FasterHandle store = faster_open();
    REQUIRE(store != INVALID_FASTER_HANDLE);
    FasterHandle sess = faster_session_start(store);
    REQUIRE(sess != INVALID_FASTER_HANDLE);

    uint8_t key = 1;
    // Non-zero val_len with null val_ptr → InvalidArgument
    FasterStatus s = faster_upsert(store, sess, &key, 1, nullptr, 10);
    CHECK(s == FasterStatus_InvalidArgument);

    faster_session_end(store, sess);
    faster_close(store);
}

TEST_CASE(read_null_output_len) {
    FasterHandle store = faster_open();
    REQUIRE(store != INVALID_FASTER_HANDLE);
    FasterHandle sess = faster_session_start(store);
    REQUIRE(sess != INVALID_FASTER_HANDLE);

    uint8_t key = 1;
    uint8_t buf[64];
    // Null val_out_len → InvalidArgument
    FasterStatus s = faster_read(store, sess, &key, 1, buf, sizeof(buf), nullptr);
    CHECK(s == FasterStatus_InvalidArgument);

    faster_session_end(store, sess);
    faster_close(store);
}

TEST_CASE(read_null_key_ptr) {
    FasterHandle store = faster_open();
    REQUIRE(store != INVALID_FASTER_HANDLE);
    FasterHandle sess = faster_session_start(store);
    REQUIRE(sess != INVALID_FASTER_HANDLE);

    uint8_t buf[64];
    uint32_t out_len = 0;
    FasterStatus s = faster_read(store, sess, nullptr, 10, buf, sizeof(buf), &out_len);
    CHECK(s == FasterStatus_InvalidArgument);

    faster_session_end(store, sess);
    faster_close(store);
}

TEST_CASE(delete_null_key_ptr) {
    FasterHandle store = faster_open();
    REQUIRE(store != INVALID_FASTER_HANDLE);
    FasterHandle sess = faster_session_start(store);
    REQUIRE(sess != INVALID_FASTER_HANDLE);

    FasterStatus s = faster_delete(store, sess, nullptr, 10);
    CHECK(s == FasterStatus_InvalidArgument);

    faster_session_end(store, sess);
    faster_close(store);
}

TEST_CASE(rmw_null_key_ptr) {
    FasterHandle store = faster_open();
    REQUIRE(store != INVALID_FASTER_HANDLE);
    FasterHandle sess = faster_session_start(store);
    REQUIRE(sess != INVALID_FASTER_HANDLE);

    uint8_t input = 1;
    FasterStatus s = faster_rmw(store, sess, nullptr, 10, &input, 1);
    CHECK(s == FasterStatus_InvalidArgument);

    faster_session_end(store, sess);
    faster_close(store);
}

TEST_CASE(rmw_null_input_ptr) {
    FasterHandle store = faster_open();
    REQUIRE(store != INVALID_FASTER_HANDLE);
    FasterHandle sess = faster_session_start(store);
    REQUIRE(sess != INVALID_FASTER_HANDLE);

    uint8_t key = 1;
    FasterStatus s = faster_rmw(store, sess, &key, 1, nullptr, 10);
    CHECK(s == FasterStatus_InvalidArgument);

    faster_session_end(store, sess);
    faster_close(store);
}

TEST_CASE(complete_pending_null_out) {
    FasterHandle store = faster_open();
    REQUIRE(store != INVALID_FASTER_HANDLE);
    FasterHandle sess = faster_session_start(store);
    REQUIRE(sess != INVALID_FASTER_HANDLE);

    FasterStatus s = faster_complete_pending(store, sess, nullptr);
    CHECK(s == FasterStatus_InvalidArgument);

    faster_session_end(store, sess);
    faster_close(store);
}

// ═══════════════════════════════════════════════════════════════════════
// Operations on invalid session handles
// ═══════════════════════════════════════════════════════════════════════

TEST_CASE(upsert_invalid_session) {
    FasterHandle store = faster_open();
    REQUIRE(store != INVALID_FASTER_HANDLE);

    uint8_t key = 1, val = 2;
    FasterStatus s = faster_upsert(store, 9999, &key, 1, &val, 1);
    CHECK(s == FasterStatus_InvalidHandle);

    faster_close(store);
}

TEST_CASE(read_invalid_session) {
    FasterHandle store = faster_open();
    REQUIRE(store != INVALID_FASTER_HANDLE);

    uint8_t key = 1, buf[64];
    uint32_t out_len = 0;
    FasterStatus s = faster_read(store, 9999, &key, 1, buf, sizeof(buf), &out_len);
    CHECK(s == FasterStatus_InvalidHandle);

    faster_close(store);
}

TEST_CASE(delete_invalid_session) {
    FasterHandle store = faster_open();
    REQUIRE(store != INVALID_FASTER_HANDLE);

    uint8_t key = 1;
    FasterStatus s = faster_delete(store, 9999, &key, 1);
    CHECK(s == FasterStatus_InvalidHandle);

    faster_close(store);
}

// ═══════════════════════════════════════════════════════════════════════
// Checkpoint/recovery errors
// ═══════════════════════════════════════════════════════════════════════

TEST_CASE(checkpoint_null_dir) {
    FasterHandle store = faster_open();
    REQUIRE(store != INVALID_FASTER_HANDLE);

    uint64_t hi = 0, lo = 0;
    FasterStatus s = faster_checkpoint(store, nullptr, 10,
        FasterCheckpointType_FoldOver, &hi, &lo);
    CHECK(s == FasterStatus_InvalidArgument);

    faster_close(store);
}

TEST_CASE(checkpoint_null_token_out) {
    FasterHandle store = faster_open();
    REQUIRE(store != INVALID_FASTER_HANDLE);

    const char* dir = "/tmp";
    FasterStatus s = faster_checkpoint(store,
        reinterpret_cast<const uint8_t*>(dir), 4,
        FasterCheckpointType_FoldOver, nullptr, nullptr);
    CHECK(s == FasterStatus_InvalidArgument);

    faster_close(store);
}

TEST_CASE(recover_null_dir) {
    FasterHandle store = faster_open();
    REQUIRE(store != INVALID_FASTER_HANDLE);

    FasterStatus s = faster_recover(store, nullptr, 10, 0, 0);
    CHECK(s == FasterStatus_InvalidArgument);

    faster_close(store);
}

TEST_CASE(recover_empty_dir) {
    FasterHandle store = faster_open();
    REQUIRE(store != INVALID_FASTER_HANDLE);

    FasterStatus s = faster_recover(store, nullptr, 0, 0, 0);
    CHECK(s == FasterStatus_InvalidArgument);

    faster_close(store);
}

// ═══════════════════════════════════════════════════════════════════════
// C++ wrapper exception safety
// ═══════════════════════════════════════════════════════════════════════

TEST_CASE(wrapper_invalid_path_throws) {
    // Opening with empty string should throw from the wrapper
    bool threw = false;
    try {
        faster::FasterKv<uint64_t, uint64_t> kv("");
    } catch (const std::runtime_error&) {
        threw = true;
    }
    CHECK(threw);
}

TEST_CASE(double_end_session_safe) {
    faster::FasterKv<uint64_t, uint64_t> kv;
    auto session = kv.StartSession();
    session.End();
    // Second End() is safe — no exception, no crash
    CHECK_NOTHROW(session.End());
}

TEST_CASE(operations_after_session_end) {
    faster::FasterKv<uint64_t, uint64_t> kv;
    auto session = kv.StartSession();
    session.End();

    // Operations on ended session should throw (invalid handle)
    CHECK_THROWS(session.Upsert(uint64_t(1), uint64_t(1)));
}

TEST_CASE(zero_length_key) {
    // Zero-length key is allowed by the FFI (maps to empty Vec<u8>)
    FasterHandle store = faster_open();
    REQUIRE(store != INVALID_FASTER_HANDLE);
    FasterHandle sess = faster_session_start(store);
    REQUIRE(sess != INVALID_FASTER_HANDLE);

    uint8_t val = 42;
    FasterStatus s = faster_upsert(store, sess, nullptr, 0, &val, 1);
    CHECK(s == FasterStatus_Ok || s == FasterStatus_Created
          || s == FasterStatus_InPlaceUpdated || s == FasterStatus_CopyUpdated);

    faster_session_end(store, sess);
    faster_close(store);
}

TEST_CASE(open_with_path_null_ptr) {
    FasterHandle h = faster_open_with_path(nullptr, 10);
    CHECK(h == INVALID_FASTER_HANDLE);
}

TEST_CASE(open_with_path_zero_len) {
    const char* p = "/tmp";
    FasterHandle h = faster_open_with_path(
        reinterpret_cast<const uint8_t*>(p), 0);
    CHECK(h == INVALID_FASTER_HANDLE);
}

// ═══════════════════════════════════════════════════════════════════════
// Buffer too small (read)
// ═══════════════════════════════════════════════════════════════════════

TEST_CASE(read_buffer_too_small) {
    FasterHandle store = faster_open();
    REQUIRE(store != INVALID_FASTER_HANDLE);
    FasterHandle sess = faster_session_start(store);
    REQUIRE(sess != INVALID_FASTER_HANDLE);

    // Write 8 bytes
    uint64_t key = 1, val = 42;
    faster_upsert(store, sess,
        reinterpret_cast<const uint8_t*>(&key), sizeof(key),
        reinterpret_cast<const uint8_t*>(&val), sizeof(val));
    faster_session_refresh(store, sess, nullptr);

    // Try to read into 1-byte buffer
    uint8_t tiny_buf[1];
    uint32_t out_len = 0;
    FasterStatus s = faster_read(store, sess,
        reinterpret_cast<const uint8_t*>(&key), sizeof(key),
        tiny_buf, 1, &out_len);
    CHECK(s == FasterStatus_BufferTooSmall);
    CHECK(out_len == sizeof(uint64_t)); // Reports needed size

    faster_session_end(store, sess);
    faster_close(store);
}

TEST_MAIN()

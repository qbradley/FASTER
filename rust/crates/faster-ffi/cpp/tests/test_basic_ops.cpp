// test_basic_ops.cpp — Integration tests for basic CRUD operations.
//
// Tests: open/close store, upsert, read, update, delete, RMW with
// various key types (uint64_t, string, vector<uint8_t>).
//
// Copyright (c) Microsoft Corporation. Licensed under the MIT License.

#include "../faster_cpp.h"
#include "test_harness.h"

#include <cstring>

// ═══════════════════════════════════════════════════════════════════════
// Store open/close
// ═══════════════════════════════════════════════════════════════════════

TEST_CASE(open_close_in_memory) {
    faster::FasterKv<uint64_t, uint64_t> kv;
    // If we get here without throwing, the store opened successfully.
    CHECK(kv.raw_handle() != 0);
}

TEST_CASE(open_close_persistent) {
    auto dir = faster_test::make_temp_dir("basic_open");
    {
        faster::FasterKv<uint64_t, uint64_t> kv(dir);
        CHECK(kv.raw_handle() != 0);
    }
    faster_test::cleanup_temp_dir(dir);
}

// ═══════════════════════════════════════════════════════════════════════
// uint64_t key/value CRUD
// ═══════════════════════════════════════════════════════════════════════

TEST_CASE(upsert_read_u64) {
    faster::FasterKv<uint64_t, uint64_t> kv;
    auto session = kv.StartSession();

    auto s = session.Upsert(uint64_t(1), uint64_t(100));
    CHECK(s == faster::Status::Ok || s == faster::Status::Created);

    session.Refresh();

    uint64_t val = 0;
    s = session.Read(uint64_t(1), val);
    CHECK(s == faster::Status::Ok);
    CHECK(val == 100);
}

TEST_CASE(upsert_overwrite_u64) {
    faster::FasterKv<uint64_t, uint64_t> kv;
    auto session = kv.StartSession();

    session.Upsert(uint64_t(1), uint64_t(100));
    session.Refresh();
    session.Upsert(uint64_t(1), uint64_t(200));
    session.Refresh();

    uint64_t val = 0;
    auto s = session.Read(uint64_t(1), val);
    CHECK(s == faster::Status::Ok);
    CHECK(val == 200);
}

TEST_CASE(read_nonexistent_u64) {
    faster::FasterKv<uint64_t, uint64_t> kv;
    auto session = kv.StartSession();

    uint64_t val = 999;
    auto s = session.Read(uint64_t(42), val);
    CHECK(s == faster::Status::NotFound);
}

TEST_CASE(delete_u64) {
    faster::FasterKv<uint64_t, uint64_t> kv;
    auto session = kv.StartSession();

    session.Upsert(uint64_t(1), uint64_t(100));
    session.Refresh();

    auto s = session.Delete(uint64_t(1));
    CHECK(s == faster::Status::Ok);

    session.Refresh();

    uint64_t val = 0;
    s = session.Read(uint64_t(1), val);
    CHECK(s == faster::Status::NotFound);
}

TEST_CASE(delete_nonexistent_u64) {
    faster::FasterKv<uint64_t, uint64_t> kv;
    auto session = kv.StartSession();

    // Deleting a key that doesn't exist should succeed (FASTER semantics:
    // delete creates a tombstone record regardless).
    auto s = session.Delete(uint64_t(999));
    CHECK(s == faster::Status::Ok || s == faster::Status::NotFound);
}

TEST_CASE(rmw_replace_u64) {
    faster::FasterKv<uint64_t, uint64_t> kv;
    auto session = kv.StartSession();

    session.Upsert(uint64_t(1), uint64_t(100));
    session.Refresh();

    auto s = session.Rmw(uint64_t(1), uint64_t(999));
    CHECK(s == faster::Status::Ok || s == faster::Status::InPlaceUpdated
          || s == faster::Status::CopyUpdated);

    session.Refresh();

    uint64_t val = 0;
    s = session.Read(uint64_t(1), val);
    CHECK(s == faster::Status::Ok);
    CHECK(val == 999);
}

TEST_CASE(rmw_insert_u64) {
    faster::FasterKv<uint64_t, uint64_t> kv;
    auto session = kv.StartSession();

    // RMW on a missing key creates it (default behavior = value replacement).
    auto s = session.Rmw(uint64_t(42), uint64_t(777));
    CHECK(s == faster::Status::Ok || s == faster::Status::Created
          || s == faster::Status::InPlaceUpdated
          || s == faster::Status::CopyUpdated);

    session.Refresh();

    uint64_t val = 0;
    s = session.Read(uint64_t(42), val);
    CHECK(s == faster::Status::Ok);
    CHECK(val == 777);
}

// ═══════════════════════════════════════════════════════════════════════
// std::string key/value CRUD
// ═══════════════════════════════════════════════════════════════════════

TEST_CASE(upsert_read_string) {
    faster::FasterKv<std::string, std::string> kv;
    auto session = kv.StartSession();

    session.Upsert(std::string("hello"), std::string("world"));
    session.Refresh();

    std::string val;
    auto s = session.Read(std::string("hello"), val);
    CHECK(s == faster::Status::Ok);
    CHECK(val == "world");
}

TEST_CASE(read_miss_string) {
    faster::FasterKv<std::string, std::string> kv;
    auto session = kv.StartSession();

    std::string val;
    auto s = session.Read(std::string("missing"), val);
    CHECK(s == faster::Status::NotFound);
}

TEST_CASE(delete_string) {
    faster::FasterKv<std::string, std::string> kv;
    auto session = kv.StartSession();

    session.Upsert(std::string("key"), std::string("value"));
    session.Refresh();

    auto s = session.Delete(std::string("key"));
    CHECK(s == faster::Status::Ok);
    session.Refresh();

    std::string val;
    s = session.Read(std::string("key"), val);
    CHECK(s == faster::Status::NotFound);
}

TEST_CASE(string_empty_value) {
    faster::FasterKv<std::string, std::string> kv;
    auto session = kv.StartSession();

    session.Upsert(std::string("k"), std::string(""));
    session.Refresh();

    std::string val = "not_empty";
    auto s = session.Read(std::string("k"), val);
    CHECK(s == faster::Status::Ok);
    CHECK(val.empty());
}

TEST_CASE(string_long_values) {
    faster::FasterKv<std::string, std::string> kv;
    auto session = kv.StartSession();

    std::string long_key(256, 'K');
    std::string long_val(4096, 'V');

    session.Upsert(long_key, long_val);
    session.Refresh();

    std::string out;
    auto s = session.Read(long_key, out);
    CHECK(s == faster::Status::Ok);
    CHECK(out == long_val);
}

// ═══════════════════════════════════════════════════════════════════════
// vector<uint8_t> key/value (raw bytes)
// ═══════════════════════════════════════════════════════════════════════

TEST_CASE(upsert_read_bytes) {
    faster::FasterKv<std::vector<uint8_t>, std::vector<uint8_t>> kv;
    auto session = kv.StartSession();

    std::vector<uint8_t> key = {0x01, 0x02, 0x03};
    std::vector<uint8_t> val = {0xCA, 0xFE, 0xBA, 0xBE};

    session.Upsert(key, val);
    session.Refresh();

    std::vector<uint8_t> out;
    auto s = session.Read(key, out);
    CHECK(s == faster::Status::Ok);
    CHECK(out == val);
}

// ═══════════════════════════════════════════════════════════════════════
// Bulk operations
// ═══════════════════════════════════════════════════════════════════════

TEST_CASE(bulk_1000_records) {
    faster::FasterKv<uint64_t, uint64_t> kv;
    auto session = kv.StartSession();

    constexpr uint64_t N = 1000;

    for (uint64_t i = 0; i < N; ++i) {
        session.Upsert(i, i * 10);
        if (i % 100 == 0) session.Refresh();
    }
    session.Refresh();

    int mismatches = 0;
    for (uint64_t i = 0; i < N; ++i) {
        uint64_t val = 0;
        auto s = session.Read(i, val);
        if (s != faster::Status::Ok || val != i * 10) ++mismatches;
        if (i % 100 == 0) session.Refresh();
    }
    CHECK(mismatches == 0);
}

TEST_CASE(bulk_delete_verify) {
    faster::FasterKv<uint64_t, uint64_t> kv;
    auto session = kv.StartSession();

    constexpr uint64_t N = 100;
    for (uint64_t i = 0; i < N; ++i) {
        session.Upsert(i, i);
        if (i % 50 == 0) session.Refresh();
    }
    session.Refresh();

    // Delete even keys
    for (uint64_t i = 0; i < N; i += 2) {
        session.Delete(i);
        if (i % 50 == 0) session.Refresh();
    }
    session.Refresh();

    int found_even = 0;
    int missing_odd = 0;
    for (uint64_t i = 0; i < N; ++i) {
        uint64_t val = 0;
        auto s = session.Read(i, val);
        if (i % 2 == 0 && s != faster::Status::NotFound) ++found_even;
        if (i % 2 == 1 && (s != faster::Status::Ok || val != i)) ++missing_odd;
    }
    CHECK(found_even == 0);
    CHECK(missing_odd == 0);
}

// ═══════════════════════════════════════════════════════════════════════
// Mixed key/value types: uint64_t key, string value
// ═══════════════════════════════════════════════════════════════════════

TEST_CASE(u64_key_string_value) {
    faster::FasterKv<uint64_t, std::string> kv;
    auto session = kv.StartSession();

    session.Upsert(uint64_t(42), std::string("the answer"));
    session.Refresh();

    std::string val;
    auto s = session.Read(uint64_t(42), val);
    CHECK(s == faster::Status::Ok);
    CHECK(val == "the answer");
}

TEST_MAIN()

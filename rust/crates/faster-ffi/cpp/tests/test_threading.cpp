// test_threading.cpp — Integration tests for multi-threaded access.
//
// Tests: concurrent sessions on the same store, thread safety of
// FasterKv, and session thread-affinity enforcement.
//
// Copyright (c) Microsoft Corporation. Licensed under the MIT License.

#include "../faster_cpp.h"
#include "test_harness.h"

#include <atomic>
#include <thread>
#include <vector>

// ═══════════════════════════════════════════════════════════════════════
// Multi-threaded upsert/read
// ═══════════════════════════════════════════════════════════════════════

TEST_CASE(concurrent_sessions_upsert_read) {
    faster::FasterKv<uint64_t, uint64_t> kv;
    constexpr int NUM_THREADS = 4;
    constexpr uint64_t PER_THREAD = 200;

    std::atomic<int> errors{0};

    auto worker = [&](int tid) {
        auto session = kv.StartSession();
        uint64_t base = static_cast<uint64_t>(tid) * PER_THREAD;

        // Upsert phase
        for (uint64_t i = 0; i < PER_THREAD; ++i) {
            session.Upsert(base + i, base + i + 1000);
            if (i % 50 == 0) session.Refresh();
        }
        session.Refresh();

        // Read-back phase
        for (uint64_t i = 0; i < PER_THREAD; ++i) {
            uint64_t val = 0;
            auto s = session.Read(base + i, val);
            if (s != faster::Status::Ok || val != base + i + 1000) {
                errors.fetch_add(1, std::memory_order_relaxed);
            }
            if (i % 50 == 0) session.Refresh();
        }
    };

    std::vector<std::thread> threads;
    for (int t = 0; t < NUM_THREADS; ++t) {
        threads.emplace_back(worker, t);
    }
    for (auto& th : threads) th.join();

    CHECK(errors.load() == 0);
}

// ═══════════════════════════════════════════════════════════════════════
// Concurrent RMW from multiple threads
// ═══════════════════════════════════════════════════════════════════════

TEST_CASE(concurrent_rmw) {
    faster::FasterKv<uint64_t, uint64_t> kv;
    constexpr int NUM_THREADS = 4;
    constexpr uint64_t ITERS = 100;

    // Each thread RMWs its own unique key range — no contention on keys
    // (contention-free to avoid relying on specific merge semantics).
    std::atomic<int> errors{0};

    auto worker = [&](int tid) {
        auto session = kv.StartSession();
        uint64_t key = static_cast<uint64_t>(tid);

        for (uint64_t i = 0; i < ITERS; ++i) {
            session.Rmw(key, i);
            if (i % 25 == 0) session.Refresh();
        }
        session.Refresh();

        // Last RMW wins (full-value replacement), so value == ITERS - 1
        uint64_t val = 0;
        auto s = session.Read(key, val);
        if (s != faster::Status::Ok || val != ITERS - 1) {
            errors.fetch_add(1, std::memory_order_relaxed);
        }
    };

    std::vector<std::thread> threads;
    for (int t = 0; t < NUM_THREADS; ++t) {
        threads.emplace_back(worker, t);
    }
    for (auto& th : threads) th.join();

    CHECK(errors.load() == 0);
}

// ═══════════════════════════════════════════════════════════════════════
// Session created and used on different threads (should detect mismatch)
// ═══════════════════════════════════════════════════════════════════════

TEST_CASE(session_thread_affinity) {
    // The FFI layer enforces thread affinity — using a session from a
    // different thread should throw (ThreadMismatch → exception).
    faster::FasterKv<uint64_t, uint64_t> kv;
    auto session = kv.StartSession();

    // Write from the owning thread first
    session.Upsert(uint64_t(1), uint64_t(42));
    session.Refresh();

    bool threw_on_other_thread = false;

    std::thread other([&] {
        try {
            // Attempt to use session from a different thread
            uint64_t val = 0;
            session.Read(uint64_t(1), val);
        } catch (const std::runtime_error& e) {
            std::string what = e.what();
            if (what.find("thread") != std::string::npos ||
                what.find("mismatch") != std::string::npos) {
                threw_on_other_thread = true;
            }
        }
    });
    other.join();

    CHECK(threw_on_other_thread);
}

// ═══════════════════════════════════════════════════════════════════════
// Many sessions created and destroyed rapidly
// ═══════════════════════════════════════════════════════════════════════

TEST_CASE(rapid_session_churn) {
    faster::FasterKv<uint64_t, uint64_t> kv;
    constexpr int NUM_THREADS = 4;
    constexpr int CHURN = 50;

    std::atomic<int> errors{0};

    auto worker = [&](int tid) {
        for (int i = 0; i < CHURN; ++i) {
            try {
                auto session = kv.StartSession();
                uint64_t key = static_cast<uint64_t>(tid * CHURN + i);
                session.Upsert(key, key);
                session.Refresh();
            } catch (...) {
                errors.fetch_add(1, std::memory_order_relaxed);
            }
        }
    };

    std::vector<std::thread> threads;
    for (int t = 0; t < NUM_THREADS; ++t) {
        threads.emplace_back(worker, t);
    }
    for (auto& th : threads) th.join();

    CHECK(errors.load() == 0);
}

// ═══════════════════════════════════════════════════════════════════════
// Multi-threaded insert, single-thread read verification
// ═══════════════════════════════════════════════════════════════════════

TEST_CASE(multithread_insert_singlethread_verify) {
    faster::FasterKv<uint64_t, uint64_t> kv;
    constexpr int NUM_THREADS = 4;
    constexpr uint64_t PER_THREAD = 100;

    auto writer = [&](int tid) {
        auto session = kv.StartSession();
        uint64_t base = static_cast<uint64_t>(tid) * PER_THREAD;
        for (uint64_t i = 0; i < PER_THREAD; ++i) {
            session.Upsert(base + i, base + i);
            if (i % 25 == 0) session.Refresh();
        }
        session.Refresh();
    };

    std::vector<std::thread> threads;
    for (int t = 0; t < NUM_THREADS; ++t) {
        threads.emplace_back(writer, t);
    }
    for (auto& th : threads) th.join();

    // Verify all records from a single session
    auto session = kv.StartSession();
    int mismatches = 0;
    for (int t = 0; t < NUM_THREADS; ++t) {
        uint64_t base = static_cast<uint64_t>(t) * PER_THREAD;
        for (uint64_t i = 0; i < PER_THREAD; ++i) {
            uint64_t val = 0;
            auto s = session.Read(base + i, val);
            if (s != faster::Status::Ok || val != base + i) ++mismatches;
        }
        session.Refresh();
    }
    CHECK(mismatches == 0);
}

// ═══════════════════════════════════════════════════════════════════════
// Concurrent deletes
// ═══════════════════════════════════════════════════════════════════════

TEST_CASE(concurrent_deletes) {
    faster::FasterKv<uint64_t, uint64_t> kv;
    constexpr uint64_t N = 200;

    // Insert all records first
    {
        auto session = kv.StartSession();
        for (uint64_t i = 0; i < N; ++i) {
            session.Upsert(i, i);
            if (i % 50 == 0) session.Refresh();
        }
        session.Refresh();
    }

    // Delete in parallel (each thread deletes its range)
    constexpr int NUM_THREADS = 4;
    constexpr uint64_t PER_THREAD = N / NUM_THREADS;

    auto deleter = [&](int tid) {
        auto session = kv.StartSession();
        uint64_t base = static_cast<uint64_t>(tid) * PER_THREAD;
        for (uint64_t i = 0; i < PER_THREAD; ++i) {
            session.Delete(base + i);
            if (i % 25 == 0) session.Refresh();
        }
        session.Refresh();
    };

    std::vector<std::thread> threads;
    for (int t = 0; t < NUM_THREADS; ++t) {
        threads.emplace_back(deleter, t);
    }
    for (auto& th : threads) th.join();

    // Verify all deleted
    auto session = kv.StartSession();
    int found = 0;
    for (uint64_t i = 0; i < N; ++i) {
        uint64_t val = 0;
        auto s = session.Read(i, val);
        if (s != faster::Status::NotFound) ++found;
    }
    CHECK(found == 0);
}

TEST_MAIN()

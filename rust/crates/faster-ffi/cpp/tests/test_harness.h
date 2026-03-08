// test_harness.h — Lightweight C++ test harness for FASTER integration tests.
//
// Provides CHECK/REQUIRE macros, test registration, and a main() generator.
// No external dependencies — just C++17.
//
// Copyright (c) Microsoft Corporation. Licensed under the MIT License.

#ifndef FASTER_TEST_HARNESS_H
#define FASTER_TEST_HARNESS_H

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <functional>
#include <string>
#include <vector>

namespace faster_test {

struct TestCase {
    std::string name;
    std::function<void()> fn;
};

inline std::vector<TestCase>& registry() {
    static std::vector<TestCase> tests;
    return tests;
}

inline int g_checks_passed = 0;
inline int g_checks_failed = 0;
inline int g_tests_passed  = 0;
inline int g_tests_failed  = 0;

struct AutoRegister {
    AutoRegister(const char* name, std::function<void()> fn) {
        registry().push_back({name, std::move(fn)});
    }
};

/// Create a unique temporary directory for a test.
inline std::string make_temp_dir(const std::string& prefix) {
    namespace fs = std::filesystem;
    auto base = fs::temp_directory_path() / ("faster_test_" + prefix);
    fs::remove_all(base);
    fs::create_directories(base);
    return base.string();
}

/// Remove a temporary directory.
inline void cleanup_temp_dir(const std::string& path) {
    std::filesystem::remove_all(path);
}

} // namespace faster_test

// ── Macros ──────────────────────────────────────────────────────────

#define TEST_CASE(name)                                                 \
    static void test_fn_##name();                                       \
    static faster_test::AutoRegister reg_##name(#name, test_fn_##name); \
    static void test_fn_##name()

#define CHECK(cond)                                                     \
    do {                                                                \
        if (!(cond)) {                                                  \
            std::fprintf(stderr, "  FAIL: %s (line %d)\n",             \
                         #cond, __LINE__);                              \
            ++faster_test::g_checks_failed;                             \
        } else {                                                        \
            ++faster_test::g_checks_passed;                             \
        }                                                               \
    } while (0)

#define CHECK_MSG(cond, msg)                                            \
    do {                                                                \
        if (!(cond)) {                                                  \
            std::fprintf(stderr, "  FAIL: %s — %s (line %d)\n",       \
                         #cond, (msg), __LINE__);                       \
            ++faster_test::g_checks_failed;                             \
        } else {                                                        \
            ++faster_test::g_checks_passed;                             \
        }                                                               \
    } while (0)

#define REQUIRE(cond)                                                   \
    do {                                                                \
        if (!(cond)) {                                                  \
            std::fprintf(stderr, "  FATAL: %s (line %d)\n",            \
                         #cond, __LINE__);                              \
            ++faster_test::g_checks_failed;                             \
            return;                                                     \
        } else {                                                        \
            ++faster_test::g_checks_passed;                             \
        }                                                               \
    } while (0)

#define CHECK_THROWS(expr)                                              \
    do {                                                                \
        bool threw = false;                                             \
        try { expr; } catch (...) { threw = true; }                     \
        if (!threw) {                                                   \
            std::fprintf(stderr, "  FAIL: expected exception from "    \
                         #expr " (line %d)\n", __LINE__);              \
            ++faster_test::g_checks_failed;                             \
        } else {                                                        \
            ++faster_test::g_checks_passed;                             \
        }                                                               \
    } while (0)

#define CHECK_NOTHROW(expr)                                             \
    do {                                                                \
        bool threw = false;                                             \
        try { expr; } catch (const std::exception& e) {                 \
            threw = true;                                               \
            std::fprintf(stderr, "  FAIL: unexpected exception from "  \
                         #expr ": %s (line %d)\n", e.what(), __LINE__);\
        }                                                               \
        if (threw) {                                                    \
            ++faster_test::g_checks_failed;                             \
        } else {                                                        \
            ++faster_test::g_checks_passed;                             \
        }                                                               \
    } while (0)

// ── Test runner main ────────────────────────────────────────────────

#define TEST_MAIN()                                                     \
    int main(int argc, char** argv) {                                   \
        (void)argc; (void)argv;                                         \
        auto& tests = faster_test::registry();                          \
        std::printf("Running %zu test(s)...\n", tests.size());         \
        for (auto& tc : tests) {                                        \
            int pre_fail = faster_test::g_checks_failed;                \
            std::printf("\n--- %s ---\n", tc.name.c_str());            \
            try {                                                       \
                tc.fn();                                                 \
            } catch (const std::exception& e) {                         \
                std::fprintf(stderr, "  EXCEPTION: %s\n", e.what());   \
                ++faster_test::g_checks_failed;                         \
            }                                                           \
            if (faster_test::g_checks_failed == pre_fail) {             \
                ++faster_test::g_tests_passed;                          \
            } else {                                                    \
                ++faster_test::g_tests_failed;                          \
            }                                                           \
        }                                                               \
        std::printf("\n========================================\n");    \
        std::printf("Tests: %d passed, %d failed\n",                   \
                    faster_test::g_tests_passed,                        \
                    faster_test::g_tests_failed);                       \
        std::printf("Checks: %d passed, %d failed\n",                  \
                    faster_test::g_checks_passed,                       \
                    faster_test::g_checks_failed);                      \
        return faster_test::g_tests_failed > 0 ? 1 : 0;                \
    }

#endif // FASTER_TEST_HARNESS_H

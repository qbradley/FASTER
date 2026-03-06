/*
 * test_faster.c — Verify that faster.h compiles and the API surface is usable.
 *
 * This file is compiled as part of the build verification for the cbindgen-
 * generated header. It does NOT link against the FASTER library (that would
 * require building the Rust cdylib and linking it). It only checks that:
 *
 *   1. faster.h is valid C11 and can be included.
 *   2. All public types, enums, and function declarations are accessible.
 *   3. Enum discriminant values match the expected ABI constants.
 *
 * Build:
 *   cc -std=c11 -Wall -Wextra -Werror -fsyntax-only \
 *      -I../include test_faster.c
 */

#include "faster.h"

#include <assert.h>
#include <stddef.h>

/* ── Enum discriminant checks (compile-time) ──────────────────────── */

_Static_assert(FasterStatus_Ok == 0, "FasterStatus_Ok must be 0");
_Static_assert(FasterStatus_NotFound == 1, "FasterStatus_NotFound must be 1");
_Static_assert(FasterStatus_Pending == 2, "FasterStatus_Pending must be 2");
_Static_assert(FasterStatus_Created == 3, "FasterStatus_Created must be 3");
_Static_assert(FasterStatus_InPlaceUpdated == 4, "InPlaceUpdated must be 4");
_Static_assert(FasterStatus_CopyUpdated == 5, "CopyUpdated must be 5");
_Static_assert(FasterStatus_InvalidHandle == 100, "InvalidHandle must be 100");
_Static_assert(FasterStatus_InvalidArgument == 101, "InvalidArgument must be 101");
_Static_assert(FasterStatus_BufferTooSmall == 102, "BufferTooSmall must be 102");
_Static_assert(FasterStatus_InternalError == 103, "InternalError must be 103");
_Static_assert(FasterStatus_CheckpointError == 104, "CheckpointError must be 104");

_Static_assert(FasterCheckpointType_FoldOver == 0, "FoldOver must be 0");
_Static_assert(FasterCheckpointType_Snapshot == 1, "Snapshot must be 1");

_Static_assert(INVALID_HANDLE == 0, "INVALID_HANDLE must be 0");

/* ── Type size / alignment checks ─────────────────────────────────── */

_Static_assert(sizeof(FasterHandle) == 8, "FasterHandle must be 8 bytes");
_Static_assert(sizeof(enum FasterStatus) == 4, "FasterStatus must be 4 bytes");
_Static_assert(sizeof(enum FasterCheckpointType) == 4, "FasterCheckpointType must be 4 bytes");

/* ── Verify struct layout ─────────────────────────────────────────── */

_Static_assert(
    sizeof(struct FasterCheckpointResult) >= 20,
    "FasterCheckpointResult must hold two u64 and one enum"
);

/* ── Verify function declarations are visible ─────────────────────── */

/* Use function pointers to confirm each function declaration is reachable.
   The compiler will error if any are undeclared or have wrong signatures. */

typedef FasterHandle (*open_fn)(void);
typedef FasterHandle (*open_path_fn)(const uint8_t *, uint32_t);
typedef enum FasterStatus (*close_fn)(FasterHandle);
typedef FasterHandle (*session_start_fn)(FasterHandle);
typedef enum FasterStatus (*session_end_fn)(FasterHandle, FasterHandle);
typedef enum FasterStatus (*upsert_fn)(FasterHandle, FasterHandle,
    const uint8_t *, uint32_t, const uint8_t *, uint32_t);
typedef enum FasterStatus (*read_fn)(FasterHandle, FasterHandle,
    const uint8_t *, uint32_t, uint8_t *, uint32_t, uint32_t *);
typedef enum FasterStatus (*delete_fn)(FasterHandle, FasterHandle,
    const uint8_t *, uint32_t);
typedef enum FasterStatus (*rmw_fn)(FasterHandle, FasterHandle,
    const uint8_t *, uint32_t, const uint8_t *, uint32_t);
typedef enum FasterStatus (*complete_fn)(FasterHandle, FasterHandle, uint32_t *);
typedef enum FasterStatus (*checkpoint_fn)(FasterHandle, const uint8_t *,
    uint32_t, enum FasterCheckpointType, uint64_t *, uint64_t *);
typedef enum FasterStatus (*recover_fn)(FasterHandle, const uint8_t *,
    uint32_t, uint64_t, uint64_t);

static void verify_api_surface(void) {
    /* Assign each FFI function to a matching pointer type.
       Compilation fails if the declarations are missing or wrong. */
    open_fn          f1  = faster_open;
    open_path_fn     f2  = faster_open_with_path;
    close_fn         f3  = faster_close;
    session_start_fn f4  = faster_session_start;
    session_end_fn   f5  = faster_session_end;
    upsert_fn        f6  = faster_upsert;
    read_fn          f7  = faster_read;
    delete_fn        f8  = faster_delete;
    rmw_fn           f9  = faster_rmw;
    complete_fn      f10 = faster_complete_pending;
    checkpoint_fn    f11 = faster_checkpoint;
    recover_fn       f12 = faster_recover;

    /* Suppress unused warnings. */
    (void)f1; (void)f2; (void)f3; (void)f4; (void)f5; (void)f6;
    (void)f7; (void)f8; (void)f9; (void)f10; (void)f11; (void)f12;
}

int main(void) {
    verify_api_surface();
    return 0;
}

// faster_cpp.h — Modern C++17 wrapper over the Rust FASTER C FFI layer.
//
// This is a header-only library that provides RAII-managed, type-safe access
// to the FASTER key-value store implemented in Rust and exposed via C FFI.
//
// Thread Safety:
//   - FasterKv instances are thread-safe (the underlying store uses epochs).
//   - Session objects are NOT thread-safe — each thread must create and use
//     its own Session via kv.StartSession(). Sharing a Session across threads
//     is undefined behavior.
//   - Multiple threads may hold sessions on the same FasterKv concurrently.
//
// Usage pattern:
//   FasterKv<uint64_t, uint64_t> kv("/tmp/faster-store");
//   auto session = kv.StartSession();
//   session.Upsert(42, 100);
//   uint64_t val;
//   auto status = session.Read(42, val);
//   session.Refresh();
//   // session and kv are destroyed automatically (RAII).
//
// Copyright (c) Microsoft Corporation. Licensed under the MIT License.

#ifndef FASTER_CPP_H
#define FASTER_CPP_H

#include <cstdint>
#include <cstring>
#include <functional>
#include <memory>
#include <optional>
#include <stdexcept>
#include <string>
#include <type_traits>
#include <vector>

// ═══════════════════════════════════════════════════════════════════════
// Section 1: C FFI declarations (matches faster.h ABI)
// ═══════════════════════════════════════════════════════════════════════
//
// We redeclare the FFI surface here instead of including faster.h so that:
//   (a) The header is self-contained — no dependency on cbindgen output.
//   (b) The _ex() functions use proper nullable function pointers instead
//       of opaque Option_* structs that cbindgen emits for Option<fn>.
//   (c) Everything is in an extern "C" block for C++ linkage.

extern "C" {

// ── Handles ─────────────────────────────────────────────────────────
using FasterHandle = uint64_t;
static constexpr FasterHandle INVALID_FASTER_HANDLE = 0;

// ── Status codes ────────────────────────────────────────────────────
enum FasterStatus : int32_t {
    FasterStatus_Ok              = 0,
    FasterStatus_NotFound        = 1,
    FasterStatus_Pending         = 2,
    FasterStatus_Created         = 3,
    FasterStatus_InPlaceUpdated  = 4,
    FasterStatus_CopyUpdated     = 5,
    FasterStatus_InvalidHandle   = 100,
    FasterStatus_InvalidArgument = 101,
    FasterStatus_BufferTooSmall  = 102,
    FasterStatus_InternalError   = 103,
    FasterStatus_CheckpointError = 104,
    FasterStatus_ThreadMismatch  = 105,
};

// ── Checkpoint type ─────────────────────────────────────────────────
enum FasterCheckpointType : int32_t {
    FasterCheckpointType_FoldOver = 0,
    FasterCheckpointType_Snapshot = 1,
};

// ── Callback function pointer types ─────────────────────────────────

/// RMW initial: initialize value for a new key.
using FasterRmwInitialFn = int32_t (*)(
    const uint8_t* key_ptr, size_t key_len,
    const uint8_t* input_ptr, size_t input_len,
    uint8_t* value_ptr, size_t* value_len);

/// RMW copy-update: create new value from old + input.
using FasterRmwCopyFn = int32_t (*)(
    const uint8_t* key_ptr, size_t key_len,
    const uint8_t* input_ptr, size_t input_len,
    const uint8_t* old_value_ptr, size_t old_value_len,
    uint8_t* new_value_ptr, size_t* new_value_len);

/// RMW atomic in-place: modify mutable-region value directly.
using FasterRmwAtomicFn = int32_t (*)(
    const uint8_t* key_ptr, size_t key_len,
    const uint8_t* input_ptr, size_t input_len,
    uint8_t* value_ptr, size_t value_len);

/// Upsert put: write new value.
using FasterUpsertPutFn = int32_t (*)(
    const uint8_t* key_ptr, size_t key_len,
    const uint8_t* input_ptr, size_t input_len,
    uint8_t* value_ptr, size_t value_len,
    size_t* actual_len);

/// Upsert atomic: in-place update of existing value.
using FasterUpsertPutAtomicFn = int32_t (*)(
    const uint8_t* key_ptr, size_t key_len,
    const uint8_t* input_ptr, size_t input_len,
    uint8_t* value_ptr, size_t value_len);

/// Read get: extract output from value.
using FasterReadGetFn = int32_t (*)(
    const uint8_t* key_ptr, size_t key_len,
    const uint8_t* value_ptr, size_t value_len,
    uint8_t* output_ptr, size_t* output_len);

/// Read atomic: same as ReadGetFn but for mutable region records.
using FasterReadGetAtomicFn = FasterReadGetFn;

// ── Core FFI functions ──────────────────────────────────────────────

FasterHandle faster_open();
FasterHandle faster_open_with_path(const uint8_t* path_ptr, uint32_t path_len);
FasterStatus faster_close(FasterHandle store);
FasterStatus faster_destroy(FasterHandle store);

FasterHandle faster_session_start(FasterHandle store);
FasterStatus faster_session_end(FasterHandle store, FasterHandle session);
FasterHandle faster_continue_session(FasterHandle store, uint64_t* serial_out);

FasterStatus faster_upsert(
    FasterHandle store, FasterHandle session,
    const uint8_t* key_ptr, uint32_t key_len,
    const uint8_t* val_ptr, uint32_t val_len);

FasterStatus faster_read(
    FasterHandle store, FasterHandle session,
    const uint8_t* key_ptr, uint32_t key_len,
    uint8_t* val_buf, uint32_t val_buf_len,
    uint32_t* val_out_len);

FasterStatus faster_delete(
    FasterHandle store, FasterHandle session,
    const uint8_t* key_ptr, uint32_t key_len);

FasterStatus faster_rmw(
    FasterHandle store, FasterHandle session,
    const uint8_t* key_ptr, uint32_t key_len,
    const uint8_t* input_ptr, uint32_t input_len);

FasterStatus faster_complete_pending(
    FasterHandle store, FasterHandle session,
    uint32_t* completed_out);

FasterStatus faster_session_refresh(
    FasterHandle store, FasterHandle session,
    uint32_t* completed_out);

FasterStatus faster_refresh(
    FasterHandle store, FasterHandle session,
    uint32_t* completed_out);

FasterStatus faster_wait_for_all_pending(
    FasterHandle store, FasterHandle session,
    uint32_t* completed_out);

FasterStatus faster_checkpoint(
    FasterHandle store,
    const uint8_t* checkpoint_dir_ptr, uint32_t checkpoint_dir_len,
    FasterCheckpointType checkpoint_type,
    uint64_t* token_high_out, uint64_t* token_low_out);

FasterStatus faster_recover(
    FasterHandle store,
    const uint8_t* checkpoint_dir_ptr, uint32_t checkpoint_dir_len,
    uint64_t token_high, uint64_t token_low);

// ── Extended functions with callback overrides ──────────────────────
//
// ABI note: Rust's Option<extern "C" fn(...)> is layout-identical to a
// nullable function pointer. We declare these with raw pointer types.

FasterStatus faster_rmw_ex(
    FasterHandle store, FasterHandle session,
    const uint8_t* key_ptr, uint32_t key_len,
    const uint8_t* input_ptr, uint32_t input_len,
    FasterRmwInitialFn initial_cb,
    FasterRmwCopyFn    copy_cb,
    FasterRmwAtomicFn  atomic_cb);

FasterStatus faster_upsert_ex(
    FasterHandle store, FasterHandle session,
    const uint8_t* key_ptr, uint32_t key_len,
    const uint8_t* input_ptr, uint32_t input_len,
    FasterUpsertPutFn       put_cb,
    FasterUpsertPutAtomicFn put_atomic_cb);

FasterStatus faster_read_ex(
    FasterHandle store, FasterHandle session,
    const uint8_t* key_ptr, uint32_t key_len,
    uint8_t* output_ptr, uint32_t output_buf_len,
    uint32_t* output_len,
    FasterReadGetFn       get_cb,
    FasterReadGetAtomicFn get_atomic_cb);

} // extern "C"

// ═══════════════════════════════════════════════════════════════════════
// Section 2: C++ wrapper types
// ═══════════════════════════════════════════════════════════════════════

namespace faster {

// ── Status ──────────────────────────────────────────────────────────

/// Strongly-typed operation result.
enum class Status {
    Ok             = FasterStatus_Ok,
    NotFound       = FasterStatus_NotFound,
    Pending        = FasterStatus_Pending,
    Created        = FasterStatus_Created,
    InPlaceUpdated = FasterStatus_InPlaceUpdated,
    CopyUpdated    = FasterStatus_CopyUpdated,
};

/// Checkpoint type (fold-over vs snapshot).
enum class CheckpointType {
    FoldOver = FasterCheckpointType_FoldOver,
    Snapshot = FasterCheckpointType_Snapshot,
};

/// 128-bit checkpoint token, split for C ABI compatibility.
struct CheckpointToken {
    uint64_t high = 0;
    uint64_t low  = 0;

    bool operator==(const CheckpointToken& o) const {
        return high == o.high && low == o.low;
    }
    bool operator!=(const CheckpointToken& o) const { return !(*this == o); }
};

/// Convert raw FFI status to the C++ Status enum.
/// Throws on error codes (>= 100) that indicate programming errors.
inline Status translate_status(FasterStatus raw) {
    switch (raw) {
    case FasterStatus_Ok:             return Status::Ok;
    case FasterStatus_NotFound:       return Status::NotFound;
    case FasterStatus_Pending:        return Status::Pending;
    case FasterStatus_Created:        return Status::Created;
    case FasterStatus_InPlaceUpdated: return Status::InPlaceUpdated;
    case FasterStatus_CopyUpdated:    return Status::CopyUpdated;
    case FasterStatus_InvalidHandle:
        throw std::runtime_error("FASTER: invalid handle");
    case FasterStatus_InvalidArgument:
        throw std::runtime_error("FASTER: invalid argument");
    case FasterStatus_BufferTooSmall:
        throw std::runtime_error("FASTER: buffer too small");
    case FasterStatus_InternalError:
        throw std::runtime_error("FASTER: internal error");
    case FasterStatus_CheckpointError:
        throw std::runtime_error("FASTER: checkpoint error");
    case FasterStatus_ThreadMismatch:
        throw std::runtime_error("FASTER: session used from wrong thread");
    default:
        throw std::runtime_error("FASTER: unknown status " + std::to_string(raw));
    }
}

// ── Serialization traits ────────────────────────────────────────────

/// Trait for types that can be serialized to/from a byte buffer.
///
/// Default specializations are provided for trivially-copyable types
/// (integers, POD structs, etc.). Users may specialize for custom types.
template <typename T, typename Enable = void>
struct Serializer {
    static_assert(std::is_trivially_copyable_v<T>,
        "Default Serializer requires trivially copyable types. "
        "Specialize faster::Serializer<T> for your type.");

    static constexpr uint32_t size(const T&) {
        return static_cast<uint32_t>(sizeof(T));
    }

    static void serialize(const T& val, uint8_t* buf) {
        std::memcpy(buf, &val, sizeof(T));
    }

    static T deserialize(const uint8_t* buf, uint32_t len) {
        T val{};
        std::memcpy(&val, buf, (len < sizeof(T)) ? len : sizeof(T));
        return val;
    }
};

/// Specialization for std::string: stores raw bytes (no NUL terminator).
template <>
struct Serializer<std::string> {
    static uint32_t size(const std::string& s) {
        return static_cast<uint32_t>(s.size());
    }

    static void serialize(const std::string& s, uint8_t* buf) {
        std::memcpy(buf, s.data(), s.size());
    }

    static std::string deserialize(const uint8_t* buf, uint32_t len) {
        return std::string(reinterpret_cast<const char*>(buf), len);
    }
};

/// Specialization for std::vector<uint8_t>: raw byte pass-through.
template <>
struct Serializer<std::vector<uint8_t>> {
    static uint32_t size(const std::vector<uint8_t>& v) {
        return static_cast<uint32_t>(v.size());
    }

    static void serialize(const std::vector<uint8_t>& v, uint8_t* buf) {
        std::memcpy(buf, v.data(), v.size());
    }

    static std::vector<uint8_t> deserialize(const uint8_t* buf, uint32_t len) {
        return std::vector<uint8_t>(buf, buf + len);
    }
};

// ── Forward declarations ────────────────────────────────────────────

template <typename K, typename V>
class FasterKv;

template <typename K, typename V>
class Session;

// ═══════════════════════════════════════════════════════════════════════
// Section 3: Session — per-thread RAII session handle
// ═══════════════════════════════════════════════════════════════════════

/// A FASTER session bound to the creating thread.
///
/// Sessions are move-only (not copyable). They must be used only from the
/// thread that called StartSession(). Dropping a Session automatically
/// calls faster_session_end().
///
/// All CRUD operations are on the Session, not on FasterKv directly,
/// because FASTER requires an active session for epoch participation.
template <typename K, typename V>
class Session {
public:
    Session(const Session&) = delete;
    Session& operator=(const Session&) = delete;

    Session(Session&& other) noexcept
        : store_(other.store_), session_(other.session_) {
        other.session_ = INVALID_FASTER_HANDLE;
    }

    Session& operator=(Session&& other) noexcept {
        if (this != &other) {
            End();
            store_ = other.store_;
            session_ = other.session_;
            other.session_ = INVALID_FASTER_HANDLE;
        }
        return *this;
    }

    ~Session() { End(); }

    /// Explicitly end the session. Called automatically by destructor.
    void End() {
        if (session_ != INVALID_FASTER_HANDLE) {
            faster_session_end(store_, session_);
            session_ = INVALID_FASTER_HANDLE;
        }
    }

    /// True if this session is still active.
    bool IsActive() const { return session_ != INVALID_FASTER_HANDLE; }

    // ── Basic CRUD ──────────────────────────────────────────────────

    /// Insert or replace a key-value pair.
    ///
    /// Returns Status::Ok, Created, InPlaceUpdated, or CopyUpdated on
    /// success. Throws on error.
    Status Upsert(const K& key, const V& value) {
        auto key_bytes = serialize_key(key);
        auto val_bytes = serialize_value(value);
        FasterStatus s = faster_upsert(
            store_, session_,
            key_bytes.data(), static_cast<uint32_t>(key_bytes.size()),
            val_bytes.data(), static_cast<uint32_t>(val_bytes.size()));
        return translate_status(s);
    }

    /// Read the value for a key.
    ///
    /// Returns Status::Ok on success (value written to `out`),
    /// Status::NotFound if the key doesn't exist, or Status::Pending
    /// if async I/O is needed (call CompletePending / WaitForAll).
    Status Read(const K& key, V& out) {
        auto key_bytes = serialize_key(key);
        // Start with a buffer sized for V; retry if too small.
        std::vector<uint8_t> buf(sizeof(V) < 64 ? 64 : sizeof(V));
        uint32_t actual_len = 0;

        FasterStatus s = faster_read(
            store_, session_,
            key_bytes.data(), static_cast<uint32_t>(key_bytes.size()),
            buf.data(), static_cast<uint32_t>(buf.size()),
            &actual_len);

        if (s == FasterStatus_BufferTooSmall) {
            buf.resize(actual_len);
            s = faster_read(
                store_, session_,
                key_bytes.data(), static_cast<uint32_t>(key_bytes.size()),
                buf.data(), static_cast<uint32_t>(buf.size()),
                &actual_len);
        }

        if (s == FasterStatus_Ok) {
            out = Serializer<V>::deserialize(buf.data(), actual_len);
        }
        return translate_status(s);
    }

    /// Read-modify-write with default semantics (full value replacement).
    ///
    /// If the key exists, replaces the value with `input`.
    /// If the key doesn't exist, creates it with `input` as the value.
    Status Rmw(const K& key, const V& input) {
        auto key_bytes = serialize_key(key);
        auto inp_bytes = serialize_value(input);
        FasterStatus s = faster_rmw(
            store_, session_,
            key_bytes.data(), static_cast<uint32_t>(key_bytes.size()),
            inp_bytes.data(), static_cast<uint32_t>(inp_bytes.size()));
        return translate_status(s);
    }

    /// Delete a key from the store.
    ///
    /// Returns Status::Ok if deleted, Status::NotFound if absent.
    Status Delete(const K& key) {
        auto key_bytes = serialize_key(key);
        FasterStatus s = faster_delete(
            store_, session_,
            key_bytes.data(), static_cast<uint32_t>(key_bytes.size()));
        return translate_status(s);
    }

    // ── RMW with custom merge callbacks ─────────────────────────────

    /// Read-modify-write with custom merge logic via C function pointers.
    ///
    /// The three callbacks define how FASTER merges the input with the
    /// existing (or absent) value:
    ///   - initial_fn: create a new value when the key doesn't exist
    ///   - copy_fn:    create a new value from old_value + input
    ///   - atomic_fn:  modify the value in-place in the mutable region
    ///
    /// Pass nullptr for any callback to use default byte-replacement.
    ///
    /// The `input` parameter is the modification delta (e.g. an increment).
    /// It is serialized with Serializer<Input> and passed to all callbacks
    /// as the input_ptr/input_len arguments.
    template <typename Input>
    Status RmwWithCallbacks(
            const K& key,
            const Input& input,
            FasterRmwInitialFn initial_fn,
            FasterRmwCopyFn    copy_fn,
            FasterRmwAtomicFn  atomic_fn) {
        auto key_bytes = serialize_key(key);
        uint8_t inp_buf[sizeof(Input)];
        Serializer<Input>::serialize(input, inp_buf);
        uint32_t inp_len = Serializer<Input>::size(input);

        FasterStatus s = faster_rmw_ex(
            store_, session_,
            key_bytes.data(), static_cast<uint32_t>(key_bytes.size()),
            inp_buf, inp_len,
            initial_fn, copy_fn, atomic_fn);
        return translate_status(s);
    }

    // ── Upsert with custom callbacks ────────────────────────────────

    /// Upsert with custom put/put-atomic logic via C function pointers.
    ///
    /// Allows controlling exactly how the value is written to the record.
    /// Pass nullptr for any callback to use default byte-replacement.
    template <typename Input>
    Status UpsertWithCallbacks(
            const K& key,
            const Input& input,
            FasterUpsertPutFn       put_fn,
            FasterUpsertPutAtomicFn put_atomic_fn) {
        auto key_bytes = serialize_key(key);
        uint8_t inp_buf[sizeof(Input)];
        Serializer<Input>::serialize(input, inp_buf);
        uint32_t inp_len = Serializer<Input>::size(input);

        FasterStatus s = faster_upsert_ex(
            store_, session_,
            key_bytes.data(), static_cast<uint32_t>(key_bytes.size()),
            inp_buf, inp_len,
            put_fn, put_atomic_fn);
        return translate_status(s);
    }

    // ── Read with custom callbacks ──────────────────────────────────

    /// Read with custom get logic via C function pointers.
    ///
    /// The callbacks control how the value is extracted from the record.
    /// Output is written to `out_buf` and `out_len` is set to actual size.
    Status ReadWithCallbacks(
            const K& key,
            uint8_t* out_buf, uint32_t out_buf_len, uint32_t* out_len,
            FasterReadGetFn       get_fn,
            FasterReadGetAtomicFn get_atomic_fn) {
        auto key_bytes = serialize_key(key);
        FasterStatus s = faster_read_ex(
            store_, session_,
            key_bytes.data(), static_cast<uint32_t>(key_bytes.size()),
            out_buf, out_buf_len, out_len,
            get_fn, get_atomic_fn);
        return translate_status(s);
    }

    // ── Session lifecycle ───────────────────────────────────────────

    /// Refresh the epoch and drain completed async operations.
    /// Call this periodically in long-running loops to allow FASTER to
    /// advance its internal epoch and free deferred resources.
    uint32_t Refresh() {
        uint32_t completed = 0;
        FasterStatus s = faster_session_refresh(store_, session_, &completed);
        translate_status(s);
        return completed;
    }

    /// Drain completed pending operations.
    /// Returns the number of operations completed.
    uint32_t CompletePending() {
        uint32_t completed = 0;
        FasterStatus s = faster_complete_pending(store_, session_, &completed);
        translate_status(s);
        return completed;
    }

    /// Block until all pending operations on this session complete.
    /// Returns the number of operations completed.
    uint32_t WaitForAll() {
        uint32_t completed = 0;
        FasterStatus s = faster_wait_for_all_pending(
            store_, session_, &completed);
        translate_status(s);
        return completed;
    }

private:
    friend class FasterKv<K, V>;

    Session(FasterHandle store, FasterHandle session)
        : store_(store), session_(session) {}

    /// Serialize a key to bytes via Serializer<K>.
    static std::vector<uint8_t> serialize_key(const K& key) {
        std::vector<uint8_t> buf(Serializer<K>::size(key));
        Serializer<K>::serialize(key, buf.data());
        return buf;
    }

    /// Serialize a value to bytes via Serializer<V>.
    static std::vector<uint8_t> serialize_value(const V& value) {
        std::vector<uint8_t> buf(Serializer<V>::size(value));
        Serializer<V>::serialize(value, buf.data());
        return buf;
    }

    FasterHandle store_;
    FasterHandle session_;
};

// ═══════════════════════════════════════════════════════════════════════
// Section 4: FasterKv — RAII store wrapper
// ═══════════════════════════════════════════════════════════════════════

/// RAII wrapper around the Rust FASTER key-value store.
///
/// Template parameters:
///   K — Key type (must be trivially copyable, or specialize Serializer<K>)
///   V — Value type (must be trivially copyable, or specialize Serializer<V>)
///
/// Thread safety:
///   The FasterKv object itself is safe to share across threads (the store
///   handle is thread-safe). However, all CRUD operations require a Session,
///   which is thread-affine. Create one Session per thread.
///
/// Example:
///   FasterKv<uint64_t, uint64_t> store("/tmp/my-store");
///   {
///       auto session = store.StartSession();
///       session.Upsert(1, 42);
///       uint64_t v;
///       session.Read(1, v);   // v == 42
///   } // session ends here
///   // store closes here
template <typename K, typename V>
class FasterKv {
public:
    /// Open an in-memory store (cannot be checkpointed).
    FasterKv() : handle_(faster_open()) {
        if (handle_ == INVALID_FASTER_HANDLE) {
            throw std::runtime_error("FASTER: failed to open in-memory store");
        }
    }

    /// Open a persistent store backed by files at `path`.
    /// The path is created if it doesn't exist.
    explicit FasterKv(const std::string& path)
        : handle_(faster_open_with_path(
              reinterpret_cast<const uint8_t*>(path.data()),
              static_cast<uint32_t>(path.size()))) {
        if (handle_ == INVALID_FASTER_HANDLE) {
            throw std::runtime_error(
                "FASTER: failed to open store at " + path);
        }
    }

    ~FasterKv() {
        if (handle_ != INVALID_FASTER_HANDLE) {
            faster_close(handle_);
            handle_ = INVALID_FASTER_HANDLE;
        }
    }

    // Non-copyable, movable.
    FasterKv(const FasterKv&) = delete;
    FasterKv& operator=(const FasterKv&) = delete;

    FasterKv(FasterKv&& other) noexcept : handle_(other.handle_) {
        other.handle_ = INVALID_FASTER_HANDLE;
    }

    FasterKv& operator=(FasterKv&& other) noexcept {
        if (this != &other) {
            if (handle_ != INVALID_FASTER_HANDLE) faster_close(handle_);
            handle_ = other.handle_;
            other.handle_ = INVALID_FASTER_HANDLE;
        }
        return *this;
    }

    // ── Session management ──────────────────────────────────────────

    /// Create a new session for the calling thread.
    /// Each thread must have its own session. The returned Session is
    /// move-only and will end automatically when destroyed.
    Session<K, V> StartSession() {
        FasterHandle sh = faster_session_start(handle_);
        if (sh == INVALID_FASTER_HANDLE) {
            throw std::runtime_error("FASTER: failed to start session");
        }
        return Session<K, V>(handle_, sh);
    }

    /// Resume/continue a session (currently creates a fresh session).
    /// Returns the session and the last serial number (currently always 0).
    std::pair<Session<K, V>, uint64_t> ContinueSession() {
        uint64_t serial = 0;
        FasterHandle sh = faster_continue_session(handle_, &serial);
        if (sh == INVALID_FASTER_HANDLE) {
            throw std::runtime_error("FASTER: failed to continue session");
        }
        return {Session<K, V>(handle_, sh), serial};
    }

    // ── Checkpoint / Recovery ───────────────────────────────────────

    /// Take a checkpoint. All active sessions should be ended or paused
    /// before calling this (behavior is store-implementation-dependent).
    ///
    /// Returns the checkpoint token that can be used for recovery.
    /// Throws on failure.
    CheckpointToken Checkpoint(
            const std::string& dir,
            CheckpointType type = CheckpointType::FoldOver) {
        CheckpointToken token;
        FasterStatus s = faster_checkpoint(
            handle_,
            reinterpret_cast<const uint8_t*>(dir.data()),
            static_cast<uint32_t>(dir.size()),
            static_cast<FasterCheckpointType>(type),
            &token.high, &token.low);
        translate_status(s);
        return token;
    }

    /// Recover from a checkpoint.
    /// All sessions MUST be ended before calling this.
    /// Pass a default-constructed token to recover the most recent checkpoint.
    void Recover(const std::string& dir, CheckpointToken token = {}) {
        FasterStatus s = faster_recover(
            handle_,
            reinterpret_cast<const uint8_t*>(dir.data()),
            static_cast<uint32_t>(dir.size()),
            token.high, token.low);
        translate_status(s);
    }

    /// Get the raw FFI handle (for advanced use or debugging).
    FasterHandle raw_handle() const { return handle_; }

private:
    FasterHandle handle_;
};

} // namespace faster

#endif // FASTER_CPP_H

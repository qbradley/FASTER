# Skill: Builder Pattern Turbofish Requirement

## When to Use

When creating a `FasterKv` instance via the builder pattern.

## Pattern

### The Turbofish is Required

```rust
// ✅ Correct — explicit type parameter
let kv = FasterKv::<SimpleFunctions<u64, u64>>::builder()
    .with_capacity(1024)
    .build();
```

```rust
// ❌ Won't compile — FasterKvBuilder is non-generic
let kv = FasterKv::builder()  // ❌ Can't infer F
    .with_capacity(1024)
    .build();
```

### Why This Happens

```rust
// FasterKv is generic over F
pub struct FasterKv<F: Functions> { /* ... */ }

// But FasterKvBuilder is NOT generic
pub struct FasterKvBuilder {  // No <F> parameter
    capacity: Option<usize>,
    // ...
}

impl FasterKvBuilder {
    pub fn build<F: Functions>(self) -> FasterKv<F> {
        // Generic parameter only appears at build()
    }
}
```

The type parameter `F` only appears at the `build()` call, which is too late for inference. The turbofish on the `::builder()` call establishes the type early.

## Common Contexts

### In doctests (most common failure case)
```rust
/// # Examples
/// ```
/// use faster_core::FasterKv;
/// use faster_core::functions::SimpleFunctions;
///
/// let kv = FasterKv::<SimpleFunctions<u64, u64>>::builder()  // ✅ Turbofish required
///     .with_capacity(1024)
///     .build();
/// ```
```

### In examples
```rust
// examples/basic.rs
fn main() {
    let kv = FasterKv::<SimpleFunctions<String, Vec<u8>>>::builder()
        .with_capacity(4096)
        .build();
}
```

### In tests
```rust
#[test]
fn test_basic_ops() {
    let kv = FasterKv::<SimpleFunctions<u64, u64>>::builder()
        .build();
    // ...
}
```

## Alternative: Type Annotation (Less Common)

```rust
// Also works, but turbofish is more common in this codebase
let kv: FasterKv<SimpleFunctions<u64, u64>> = FasterKv::builder()
    .with_capacity(1024)
    .build();
```

## Error Message

When you forget the turbofish:
```
error[E0282]: type annotations needed
  --> src/main.rs:10:14
   |
10 |     let kv = FasterKv::builder()
   |              ^^^^^^^^^^^^^^^^^^^ cannot infer type of the type parameter `F`
```

## Confidence

High

## Learned From

- History.md line 39: "Doctest type inference pitfall: `FasterKv::builder()` returns `FasterKvBuilder` (non-generic). Doctests must use turbofish"
- Core context: "Builder pattern: `FasterKv::<SimpleFunctions<K,V>>::builder()` — turbofish required because `FasterKvBuilder` is non-generic"

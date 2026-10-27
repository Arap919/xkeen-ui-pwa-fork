# Local compatibility patch

Source: coarsetime 0.1.38 from crates.io, licensed under BSD-2-Clause (see LICENSE).

The only source changes replace the AtomicU64 imports in src/clock.rs and
src/instant.rs with portable_atomic. Cargo.toml adds portable-atomic with its
default fallback feature. This preserves 64-bit cached timestamps on 32-bit
MIPS targets that lack native 64-bit atomics. The original API is unchanged.

Regression check from backend/:

```sh
cargo +nightly check -Z build-std=std,panic_abort --target mips-unknown-linux-musl -p coarsetime
cargo +nightly check -Z build-std=std,panic_abort --target mipsel-unknown-linux-musl -p coarsetime
```

Remove this patch when upstream supports targets without AtomicU64.

# Vendored: stylo_derive 0.19.0 (patched)

Byte-for-byte copy of the `stylo_derive` 0.19.0 crate from crates.io
(MPL-2.0, The Servo Project Developers) with one local fix in `to_css.rs`:

The ToCss derive emitted blocks ending in an untyped `Ok(())` that get the
`?` operator applied. The error type of that `Ok` is unconstrained, and
inference picks `fmt::Error` only while the dependency graph contains no
other `impl From<..> for fmt::Error`. Our graph does (gpui → zed's log
`kv_unstable_serde` feature → value-bag → serde_fmt), which makes every
`#[derive(ToCss)]` in stylo fail with E0282/E0283. The patch qualifies
those literals as `::std::fmt::Result::Ok(())`.

Wired up via `[patch.crates-io]` in the workspace `Cargo.toml`. Remove when
stylo ships an equivalent fix (check whether `cargo build` succeeds with the
patch entry deleted).

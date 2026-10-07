# Vendored patch

`webp-animation` 0.10.0 is vendored here with one fix applied on top of the
crates.io release. The parent workspace points at it through
`[patch.crates-io]` in the root `Cargo.toml`.

## The bug

`EncodingConfig` declares a `method` field — libwebp's quality/speed trade-off,
where 0 is fastest and 6 slowest — and defaults it to 4. It is never copied into
libwebp, though: `EncodingConfig::apply_to` writes only `lossless` and `quality`.
The field is therefore inert, and every caller is stuck on libwebp's own default
of 4 no matter what they set.

The reason is structural: `method` lives on `EncodingConfig`, but the config that
reaches libwebp is built by `LossyEncodingConfig::apply_to`, which copies its own
fields and cannot see `method`.

## The fix

`src/encoder_config.rs`, in `EncodingConfig::apply_to`:

```rust
webp_config.quality = self.quality;
webp_config.method = self.method as i32;
```

`libwebp-sys2` already exposes `WebPConfig::method`, so no binding work is
needed. `src/commands/sticker/lottie.rs` sets `method = 0`, which is about 2.5x
faster than the unreachable-by-accident default of 4 on the Lottie path — the
frame-rate ladder absorbs the slightly larger output.

A `test_config_method_is_applied` test covers the fix; the crate's existing
`test_config_defaults` still passes, since the default stays 4 and matches
libwebp's.

## Keeping this current

This should become an upstream PR (`blaind/webp-animation`). Once a release
carries the fix, delete this directory and the `[patch.crates-io]` entry in the
root `Cargo.toml`.

## What is omitted from the original crate

The binary (`src/main.rs`), the `examples/` tree, and the dev-dependencies only
they needed (`imageproc`, `env_logger`) are not copied. The `data/` fixtures are,
because the crate's own decoder/encoder tests read them.

## Running its tests

The crate is deliberately not a workspace member, so the parent workspace's fmt
and clippy gates keep judging only this repository's code. Run its tests from
here:

```bash
cd vendor/webp-animation && cargo test
```

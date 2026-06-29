# CLAUDE.md

## Lessons

- Use `env.example` instead of `.env.example` for the env template file.
- When using axum `Path<T>` extractor with custom ID newtypes that have a `prefix:UUID` Display format, use `Path<String>` and parse manually — axum's Path uses serde Deserialize, not FromStr, so it expects raw UUIDs.
- `missing_debug_implementations = "deny"` workspace lint requires all types to derive Debug. Add `#[derive(Debug)]` or `#[allow(dead_code)]` to internal structs.
- Clippy pedantic `unused_async` fires on test helper functions that don't actually await. Make them sync.
- Clippy pedantic `assigning_clones` prefers `field.clone_from(&source)` over `field = source.clone()` for String fields.
- Clippy pedantic `option_map_unwrap_or_else` prefers `map_or_else(else_fn, map_fn)` over `.map(f).unwrap_or_else(g)`.
- `unicode-normalization` crate's `.nfc()` iterator returns `impl Iterator<Item=char>`, so use `.collect()` to get a String.
- `globset::Glob::new(pattern)?.compile_matcher()` returns a `GlobMatcher`, not a `Glob`. To build a `GlobSet`, pass the `Glob` (not the matcher) to `GlobSetBuilder::add()`.
- Built-in profiles should NOT include `dist/` as `generated` globally — the design says to prompt before applying, since some repos intentionally commit built artifacts.
- Insta snapshot tests need `INSTA_UPDATE=always` on first run to generate `.snap` files, then run again without the flag to verify.

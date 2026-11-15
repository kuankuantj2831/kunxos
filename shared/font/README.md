# kunxfont

KunxOS's independent TrueType parser and grayscale rasterizer. The implementation
uses the OpenType `cmap`, `head`, `hhea`, `hmtx`, `maxp`, `loca`, `glyf` and
`OS/2` specifications. It does not contain or depend on fontdue, ttf-parser,
hashbrown or libm. It uses Rust's existing `core` and `alloc` libraries.

Supported:

- Standalone TrueType SFNT, Unicode cmap format 4 and 12, including supplementary
  Unicode planes, horizontal advances and USE_TYPO_METRICS line positioning.
- Simple contours, repeated point flags, implied on-curve points and quadratic
  curves; composite outlines with affine transforms and point attachment.
- Adaptive quadratic subdivision and nonzero winding scan conversion with 16
  vertical samples and analytic horizontal span coverage. Bitmaps are grayscale,
  top row first; xmin/ymin are baseline-relative, with Y pointing up.
- Immutable font bytes shared through Arc. Outline construction is lazy and
  concurrent calls do not mutate shared state.

Not implemented: CFF, WOFF, collections, variable font interpolation, font
hinting instructions, shaping, kerning and color glyph formats. Unsupported
container/variation formats return errors. Current system text and Phosphor
fonts use the supported static TrueType outlines. Font assets remain external.

Parsing validates table ranges, glyph locations, mapping ranges and outline
reads. Limits bound font size (64 MiB), glyph points (32768), composite work
(2048 nodes), recursion (16 levels), edge count (65536), bitmap dimensions and
scan work. Pixel sizes must be finite and in (0, 512]. Rendering malformed or
over-budget glyphs returns an error; shared/font_render retains its fallback.

Validation:

```powershell
cargo test --manifest-path shared/font/Cargo.toml --offline
cargo test --release --manifest-path tests/font_startup_host/Cargo.toml --offline
cargo check --manifest-path shared/font/Cargo.toml --target riscv64gc-unknown-none-elf --offline
```

The host suite uses captured upstream reference data, not a linked external
font engine. It also exercises all mapped glyphs in the four full source fonts.
Self-contained synthetic tests cover exact filled/hollow geometry, compound
transforms and attachments, cmap edge cases, cycles and malformed input.

Native applications use the reclaiming allocator in `shared/runtime` (kunxrt).
This font crate does not install a global allocator; callers own that choice.

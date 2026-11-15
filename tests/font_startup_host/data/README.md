# Font reference fixtures

Captured with upstream fontdue 0.9.4 before its removal from target dependencies.
Tests link only KunxOS's own `kunxfont` library.

Fonts in index order: cn, bold, oobe-cn, oobe-bold, launcher-cn, launcher-bold,
icon-fill, icon-line (from rootfs/fonts). Mapping text rows: font index, Unicode
scalar in decimal, glyph index. The mapping covers every mapped character.

Raster records: font index (u8), pixel size (u8), Unicode scalar (u32 LE), xmin
(i32 LE), ymin (i32 LE), width and height (u16 LE each), advance (f32 LE), then
width*height grayscale bytes, top row first. Sizes are 12, 16, 28. Samples include
ASCII, Chinese UI text, accented letters, icon glyphs and every seventh mapped
character. Their purpose is to preserve layout and detect lost strokes/holes;
the independent rasterizer has its own coverage rounding.

The original fonts and icon designs remain external assets. These fixtures do
not make those assets self-developed.

# KunxOS fontdue patch

Based on fontdue 0.9.4; original licenses retained. The optional `lazy` feature
stores validated font bytes and constructs immutable glyph geometry on first use.
Atomic publication allows concurrent calls without holding a parsing lock. The
public metrics and rasterization algorithms are unchanged. Applications enable
this feature; the kernel continues to use upstream eager parsing.

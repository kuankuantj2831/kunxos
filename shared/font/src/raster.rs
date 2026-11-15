use super::{outline::{Point, Shape}, Error, Metrics, Result};
use alloc::{vec, vec::Vec};

const MAX_EDGES: usize = 65536;
const SAMPLES: usize = 16;

#[derive(Clone, Copy)]
struct Edge { a: Point, b: Point }

struct Builder {
    edges: Vec<Edge>,
    bounds: Option<[f32; 4]>,
}

fn midpoint(a: Point, b: Point) -> Point {
    Point { x: (a.x + b.x) * 0.5, y: (a.y + b.y) * 0.5, on: true }
}

impl Builder {
    fn bound(&mut self, p: Point) {
        if let Some(b) = &mut self.bounds {
            b[0] = b[0].min(p.x); b[1] = b[1].min(p.y);
            b[2] = b[2].max(p.x); b[3] = b[3].max(p.y);
        } else { self.bounds = Some([p.x, p.y, p.x, p.y]); }
    }
    fn line(&mut self, a: Point, b: Point) -> Result<()> {
        if self.edges.len() == MAX_EDGES { return Err(Error::ComplexityLimit); }
        self.bound(a); self.bound(b);
        if a.y != b.y { self.edges.push(Edge { a, b }); }
        Ok(())
    }
    fn curve(&mut self, a: Point, control: Point, b: Point) -> Result<()> {
        self.bound(a); self.bound(b);
        // Evaluate true extrema so bitmap origins do not depend on tessellation.
        for (p, q, r) in [(a.x, control.x, b.x), (a.y, control.y, b.y)] {
            let divisor = p - 2.0 * q + r;
            if divisor == 0.0 { continue; }
            let t = (p - q) / divisor;
            if t > 0.0 && t < 1.0 {
                let u = 1.0 - t;
                self.bound(Point {
                    x: u * u * a.x + 2.0 * u * t * control.x + t * t * b.x,
                    y: u * u * a.y + 2.0 * u * t * control.y + t * t * b.y,
                    on: true,
                });
            }
        }
        self.flatten(a, control, b, 0)
    }
    fn flatten(&mut self, a: Point, c: Point, b: Point, depth: usize) -> Result<()> {
        // Control-to-chord midpoint deviation bounds the quadratic error.
        let dx = (a.x - 2.0 * c.x + b.x).abs();
        let dy = (a.y - 2.0 * c.y + b.y).abs();
        if dx.max(dy) <= 0.25 || depth == 12 { return self.line(a, b); }
        let ac = midpoint(a, c);
        let cb = midpoint(c, b);
        let middle = midpoint(ac, cb);
        self.flatten(a, ac, middle, depth + 1)?;
        self.flatten(middle, cb, b, depth + 1)
    }
    fn contour(&mut self, points: &[Point], scale: f32) -> Result<()> {
        if points.is_empty() { return Ok(()); }
        let transform = |p: Point| Point { x: p.x * scale, y: p.y * scale, on: p.on };
        let first = transform(points[0]);
        let last = transform(points[points.len() - 1]);
        let start = if first.on { first } else if last.on { last } else { midpoint(first, last) };
        let mut current = start;
        let mut control = None;
        let skip = if first.on { 1 } else { 0 };
        let take = points.len() - skip - if !first.on && last.on { 1 } else { 0 };
        for p in points[skip..skip + take].iter().copied().map(transform).chain(core::iter::once(start)) {
            if p.on {
                if let Some(c) = control.take() { self.curve(current, c, p)?; }
                else { self.line(current, p)?; }
                current = p;
            } else {
                if let Some(c) = control {
                    let mid = midpoint(c, p);
                    self.curve(current, c, mid)?;
                    current = mid;
                }
                control = Some(p);
            }
        }
        Ok(())
    }
}

fn floor(x: f32) -> i32 {
    let i = x as i32;
    if x < i as f32 { i - 1 } else { i }
}
fn ceil(x: f32) -> i32 { -floor(-x) }

pub(super) fn render(shape: &Shape, scale: f32, advance: f32) -> Result<(Metrics, Vec<u8>)> {
    let mut builder = Builder { edges: Vec::new(), bounds: None };
    let mut start = 0;
    for &end in &shape.ends {
        builder.contour(&shape.points[start..=end], scale)?;
        start = end + 1;
    }
    let mut metrics = Metrics { advance_width: advance, ..Metrics::default() };
    let Some(bounds) = builder.bounds else { return Ok((metrics, Vec::new())); };
    if bounds.iter().any(|value| !value.is_finite() || value.abs() > 65536.0) {
        return Err(Error::ComplexityLimit);
    }
    metrics.xmin = floor(bounds[0]);
    metrics.ymin = floor(bounds[1]);
    metrics.width = (ceil(bounds[2]) - metrics.xmin) as usize;
    metrics.height = (ceil(bounds[3]) - metrics.ymin) as usize;
    if metrics.width > 2048 || metrics.height > 2048
        || metrics.width * metrics.height > 1024 * 1024 {
        return Err(Error::ComplexityLimit);
    }
    let (width, height) = (metrics.width, metrics.height);
    let mut pixels = vec![0; width * height];
    if width == 0 || height == 0 { return Ok((metrics, pixels)); }
    // Bound scan work as well as memory; hostile outlines must not hang a frame.
    if builder.edges.len().saturating_mul(height).saturating_mul(SAMPLES) > 16_000_000 {
        return Err(Error::ComplexityLimit);
    }
    let mut crossings: Vec<(f32, i32)> = Vec::with_capacity(builder.edges.len());
    let mut coverage = vec![0.0f32; width];
    for row in 0..height {
        coverage.fill(0.0);
        for sample in 0..SAMPLES {
            crossings.clear();
            let y = metrics.ymin as f32 + (height - row) as f32
                - (sample as f32 + 0.5) / SAMPLES as f32;
            for edge in &builder.edges {
                let (a, b) = (edge.a, edge.b);
                if (a.y <= y && y < b.y) || (b.y <= y && y < a.y) {
                    let x = a.x + (y - a.y) * (b.x - a.x) / (b.y - a.y);
                    crossings.push((x, if b.y > a.y { 1 } else { -1 }));
                }
            }
            crossings.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
            let mut winding = 0;
            let mut left = 0.0;
            for &(right, delta) in &crossings {
                if winding != 0 {
                    span(&mut coverage, left - metrics.xmin as f32, right - metrics.xmin as f32);
                }
                winding += delta;
                left = right;
            }
        }
        for (x, value) in coverage.iter().enumerate() {
            pixels[row * width + x] = (value * (255.0 / SAMPLES as f32) + 0.5).clamp(0.0, 255.0) as u8;
        }
    }
    Ok((metrics, pixels))
}

fn span(coverage: &mut [f32], left: f32, right: f32) {
    let left = left.max(0.0);
    let right = right.min(coverage.len() as f32);
    if right <= left { return; }
    let a = floor(left) as usize;
    let b = (ceil(right) as usize).min(coverage.len());
    for (i, pixel) in coverage.iter_mut().enumerate().take(b).skip(a) {
        *pixel += right.min(i as f32 + 1.0) - left.max(i as f32);
    }
}

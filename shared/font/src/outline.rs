use super::{bytes, i16_at, u16_at, Error, Font, Result};
use alloc::vec::Vec;

const MAX_POINTS: usize = 32768;
const MAX_COMPONENTS: usize = 2048;

#[derive(Clone, Copy, Debug)]
pub(super) struct Point {
    pub x: f32,
    pub y: f32,
    pub on: bool,
}

pub(super) struct Shape {
    pub points: Vec<Point>,
    pub ends: Vec<usize>,
}

impl Shape {
    fn empty() -> Self { Self { points: Vec::new(), ends: Vec::new() } }
}

struct Reader<'a> { data: &'a [u8], pos: usize }
impl<'a> Reader<'a> {
    fn byte(&mut self) -> Result<u8> {
        let value = *self.data.get(self.pos).ok_or(Error::InvalidFont)?;
        self.pos += 1;
        Ok(value)
    }
    fn word(&mut self) -> Result<u16> {
        let value = u16_at(self.data, self.pos)?;
        self.pos += 2;
        Ok(value)
    }
    fn signed(&mut self) -> Result<i16> { Ok(self.word()? as i16) }
    fn skip(&mut self, len: usize) -> Result<()> {
        bytes(self.data, self.pos, len)?;
        self.pos += len;
        Ok(())
    }
    fn factor(&mut self) -> Result<f32> { Ok(self.signed()? as f32 / 16384.0) }
}

pub(super) fn decode(font: &Font, id: u16) -> Result<Shape> {
    let mut budget = MAX_COMPONENTS;
    glyph(font, id, 0, &mut budget)
}

fn glyph(font: &Font, id: u16, depth: usize, budget: &mut usize) -> Result<Shape> {
    if depth > 16 || *budget == 0 { return Err(Error::ComplexityLimit); }
    *budget -= 1;
    let data = font.glyph_data(id)?;
    if data.is_empty() { return Ok(Shape::empty()); }
    bytes(data, 0, 10)?;
    let count = i16_at(data, 0)?;
    if count >= 0 { return simple(data, count as usize); }
    if count != -1 { return Err(Error::InvalidFont); }
    let mut reader = Reader { data, pos: 10 };
    let mut shape = Shape::empty();
    let final_flags = loop {
        let flags = reader.word()?;
        let child_id = reader.word()?;
        let words = flags & 1 != 0;
        let xy = flags & 2 != 0;
        let (arg1, arg2) = if words {
            let (a, b) = (reader.word()?, reader.word()?);
            if xy { (a as i16 as i32, b as i16 as i32) } else { (a as i32, b as i32) }
        } else {
            let (a, b) = (reader.byte()?, reader.byte()?);
            if xy { (a as i8 as i32, b as i8 as i32) } else { (a as i32, b as i32) }
        };
        if (flags & 0x08 != 0) as u8 + (flags & 0x40 != 0) as u8
            + (flags & 0x80 != 0) as u8 > 1 || flags & 0x1800 == 0x1800 {
            return Err(Error::InvalidFont);
        }
        let (mut a, mut b, mut c, mut d) = (1.0, 0.0, 0.0, 1.0);
        if flags & 0x08 != 0 { a = reader.factor()?; d = a; }
        else if flags & 0x40 != 0 { a = reader.factor()?; d = reader.factor()?; }
        else if flags & 0x80 != 0 {
            a = reader.factor()?; b = reader.factor()?;
            c = reader.factor()?; d = reader.factor()?;
        }
        let mut child = glyph(font, child_id, depth + 1, budget)?;
        for point in &mut child.points {
            let (x, y) = (point.x, point.y);
            point.x = a * x + c * y;
            point.y = b * x + d * y;
        }
        let (mut dx, mut dy) = if xy {
            let (x, y) = (arg1 as f32, arg2 as f32);
            if flags & 0x0800 != 0 { (a * x + c * y, b * x + d * y) }
            else { (x, y) }
        } else {
            let parent = shape.points.get(arg1 as usize).ok_or(Error::InvalidFont)?;
            let child = child.points.get(arg2 as usize).ok_or(Error::InvalidFont)?;
            (parent.x - child.x, parent.y - child.y)
        };
        if xy && flags & 4 != 0 {
            dx = round(dx); dy = round(dy);
        }
        if child.points.len() + shape.points.len() > MAX_POINTS { return Err(Error::ComplexityLimit); }
        for point in &mut child.points {
            point.x += dx; point.y += dy;
            if point.x.abs() > 1_000_000.0 || point.y.abs() > 1_000_000.0 {
                return Err(Error::ComplexityLimit);
            }
        }
        let base = shape.points.len();
        shape.ends.extend(child.ends.iter().map(|end| base + end));
        shape.points.extend(child.points);
        if flags & 0x20 == 0 { break flags; }
    };
    if final_flags & 0x100 != 0 {
        let len = reader.word()? as usize;
        reader.skip(len)?;
    }
    Ok(shape)
}

fn round(value: f32) -> f32 {
    if value < 0.0 { ((value - 0.5) as i32) as f32 }
    else { ((value + 0.5) as i32) as f32 }
}

fn simple(data: &[u8], count: usize) -> Result<Shape> {
    let mut shape = Shape::empty();
    if count == 0 { return Ok(shape); }
    if count > MAX_POINTS { return Err(Error::ComplexityLimit); }
    let mut reader = Reader { data, pos: 10 };
    for _ in 0..count {
        let end = reader.word()? as usize;
        if shape.ends.last().is_some_and(|previous| end <= *previous) { return Err(Error::InvalidFont); }
        shape.ends.push(end);
    }
    let points = shape.ends[count - 1] + 1;
    if points > MAX_POINTS { return Err(Error::ComplexityLimit); }
    let instructions = reader.word()? as usize;
    reader.skip(instructions)?;
    let mut flags = Vec::with_capacity(points);
    while flags.len() < points {
        let flag = reader.byte()?;
        let repeat = if flag & 8 != 0 { reader.byte()? as usize } else { 0 };
        if flags.len() + 1 + repeat > points { return Err(Error::InvalidFont); }
        flags.resize(flags.len() + repeat + 1, flag);
    }
    let mut x = 0i32;
    for &flag in &flags {
        x += if flag & 2 != 0 {
            let delta = reader.byte()? as i32;
            if flag & 16 != 0 { delta } else { -delta }
        } else if flag & 16 != 0 { 0 } else { reader.signed()? as i32 };
        if !(-32768..=32767).contains(&x) { return Err(Error::InvalidFont); }
        shape.points.push(Point { x: x as f32, y: 0.0, on: flag & 1 != 0 });
    }
    let mut y = 0i32;
    for (&flag, point) in flags.iter().zip(&mut shape.points) {
        y += if flag & 4 != 0 {
            let delta = reader.byte()? as i32;
            if flag & 32 != 0 { delta } else { -delta }
        } else if flag & 32 != 0 { 0 } else { reader.signed()? as i32 };
        if !(-32768..=32767).contains(&y) { return Err(Error::InvalidFont); }
        point.y = y as f32;
    }
    Ok(shape)
}

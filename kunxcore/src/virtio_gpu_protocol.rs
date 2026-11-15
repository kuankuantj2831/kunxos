//! Little-endian virtio-gpu 2D wire protocol (24-byte control header).
//! Reference: QEMU include/standard-headers/linux/virtio_gpu.h.

pub const GET_DISPLAY_INFO: u32 = 0x100;
pub const CREATE_2D: u32 = 0x101;
pub const UNREF: u32 = 0x102;
pub const SET_SCANOUT: u32 = 0x103;
pub const FLUSH: u32 = 0x104;
pub const TRANSFER: u32 = 0x105;
pub const ATTACH: u32 = 0x106;
pub const OK_NODATA: u32 = 0x1100;
pub const OK_DISPLAY_INFO: u32 = 0x1101;
pub const HEADER_LEN: usize = 24;
pub const DISPLAY_INFO_LEN: usize = HEADER_LEN + 16 * 24;

pub struct Command {
    pub bytes: [u8; 64],
    pub len: usize,
}

impl Command {
    pub fn new(kind: u32, words: &[u32]) -> Self {
        assert!(words.len() <= 10);
        let mut cmd = Self { bytes: [0; 64], len: HEADER_LEN + words.len() * 4 };
        cmd.bytes[..4].copy_from_slice(&kind.to_le_bytes());
        // Completion must fence host processing, not just dequeue the request.
        cmd.bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
        for (i, word) in words.iter().enumerate() {
            cmd.bytes[HEADER_LEN + i * 4..HEADER_LEN + i * 4 + 4]
                .copy_from_slice(&word.to_le_bytes());
        }
        cmd
    }

    pub fn fence(&mut self, id: u64) {
        self.bytes[8..16].copy_from_slice(&id.to_le_bytes());
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Damage {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Damage {
    pub fn clip(x: i32, y: i32, w: u32, h: u32, sw: u32, sh: u32) -> Option<Self> {
        let x0 = (x as i64).max(0);
        let y0 = (y as i64).max(0);
        let x1 = (x as i64 + w as i64).min(sw as i64);
        let y1 = (y as i64 + h as i64).min(sh as i64);
        (x1 > x0 && y1 > y0).then(|| Self {
            x: x0 as u32, y: y0 as u32, w: (x1 - x0) as u32, h: (y1 - y0) as u32,
        })
    }

    pub fn union(self, rhs: Self) -> Self {
        let x = self.x.min(rhs.x);
        let y = self.y.min(rhs.y);
        Self { x, y, w: (self.x + self.w).max(rhs.x + rhs.w) - x,
            h: (self.y + self.h).max(rhs.y + rhs.h) - y }
    }

    pub fn transfer(self, resource: u32, stride: u32) -> Command {
        let offset = self.y as u64 * stride as u64 + self.x as u64 * 4;
        Command::new(TRANSFER, &[self.x, self.y, self.w, self.h,
            offset as u32, (offset >> 32) as u32, resource, 0])
    }

    pub fn flush(self, resource: u32) -> Command {
        Command::new(FLUSH, &[self.x, self.y, self.w, self.h, resource, 0])
    }

    pub fn scanout(self, scanout: u32, resource: u32) -> Command {
        Command::new(SET_SCANOUT, &[self.x, self.y, self.w, self.h, scanout, resource])
    }
}

pub fn create(resource: u32, width: u32, height: u32) -> Command {
    // B8G8R8X8_UNORM: bytes B,G,R,X match the existing XRGB8888 canvas.
    Command::new(CREATE_2D, &[resource, 2, width, height])
}

pub fn attach(resource: u32, pa: u64, len: u32) -> Command {
    Command::new(ATTACH, &[resource, 1, pa as u32, (pa >> 32) as u32, len, 0])
}

#[cfg(test)]
mod tests {
    use super::*;
    fn word(cmd: &Command, offset: usize) -> u32 {
        u32::from_le_bytes(cmd.bytes[offset..offset + 4].try_into().unwrap())
    }

    #[test]
    fn resource_layout_and_64_bit_backing_address() {
        let c = create(7, 540, 960);
        assert_eq!((c.len, word(&c, 0), word(&c, 24), word(&c, 28)), (40, CREATE_2D, 7, 2));
        assert_eq!((word(&c, 32), word(&c, 36)), (540, 960));
        let a = attach(7, 0x1234_8000_0000, 540 * 960 * 4);
        assert_eq!(a.len, 48);
        assert_eq!((word(&a, 28), word(&a, 32), word(&a, 36)), (1, 0x8000_0000, 0x1234));
        assert_eq!(word(&a, 40), 540 * 960 * 4);
    }

    #[test]
    fn partial_transfer_uses_full_resource_stride() {
        let d = Damage { x: 17, y: 11, w: 31, h: 9 };
        let c = d.transfer(2, 1920 * 4);
        assert_eq!(c.len, 56);
        assert_eq!(word(&c, 40), 11 * 1920 * 4 + 17 * 4);
        assert_eq!((word(&c, 44), word(&c, 48)), (0, 2));
        assert_eq!((d.flush(2).len, d.scanout(0, 2).len), (48, 48));
    }

    #[test]
    fn clipping_rejects_empty_and_offscreen_without_overflow() {
        assert_eq!(Damage::clip(-10, -20, 40, 50, 540, 960),
            Some(Damage { x: 0, y: 0, w: 30, h: 30 }));
        assert_eq!(Damage::clip(539, 959, u32::MAX, u32::MAX, 540, 960),
            Some(Damage { x: 539, y: 959, w: 1, h: 1 }));
        assert_eq!(Damage::clip(600, 0, 20, 20, 540, 960), None);
        assert_eq!(Damage::clip(0, 0, 0, 20, 540, 960), None);
        assert_eq!(Damage::clip(i32::MIN, 0, 1, 1, 540, 960), None);
    }

    #[test]
    fn damage_union_and_fenced_header() {
        let a = Damage { x: 10, y: 20, w: 30, h: 40 };
        let b = Damage { x: 100, y: 5, w: 7, h: 9 };
        assert_eq!(a.union(b), Damage { x: 10, y: 5, w: 97, h: 55 });
        let mut c = Command::new(GET_DISPLAY_INFO, &[]);
        c.fence(0x1234_5678_9abc_def0);
        assert_eq!(word(&c, 4), 1);
        assert_eq!(&c.bytes[8..16], &0x1234_5678_9abc_def0u64.to_le_bytes());
        assert_eq!(&c.bytes[16..24], &[0; 8]);
        assert_eq!(DISPLAY_INFO_LEN, 408);
    }
}

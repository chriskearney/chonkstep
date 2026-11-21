//! SFGOP connection is kept alive until all mapped writes have stopped.
use crate::damage::DRect;
use crate::pixels::{FramebufferLayout, PackTables};
use std::time::Instant;
use wm_core::Size;

/// What one present call cost. All counters are for that call only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PresentStats {
    /// RGBA to native pixel conversion.
    pub convert_us: u64,
    /// Stores into the framebuffer mapping.
    pub present_us: u64,
    pub bytes_written: u64,
    /// Bytes of large temporary buffers allocated for this call.
    pub large_alloc_bytes: u64,
}

pub trait Presenter {
    /// Show the complete frame.
    fn present(&mut self, frame: &tiny_skia::Pixmap) -> Result<(), String>;
    /// Show `frame`, of which only `damage` differs from what this presenter
    /// was last given. `frame` is always the complete, current image, so a
    /// presenter without partial updates may show all of it (the default).
    fn present_damage(&mut self, frame: &tiny_skia::Pixmap, damage: &[DRect]) -> Result<(), String> {
        let _ = damage;
        self.present(frame)
    }
    /// Cost of the last successful present, if this presenter measures it.
    fn take_stats(&mut self) -> Option<PresentStats> {
        None
    }
}

/// A validated 32-bit framebuffer mapping and both ways of filling it. The
/// owner of the memory (SFGOP mapping or a heap buffer) wraps this.
pub struct Scanout {
    base: *mut u8,
    layout: FramebufferLayout,
    tables: PackTables,
    /// Reused conversion buffer: up to `BAND_ROWS` full rows.
    band: Vec<u8>,
}
/// Rows converted before they are copied out. Small enough to stay in cache,
/// large enough that the two clock readings per band cost nothing.
const BAND_ROWS: usize = 16;
impl Scanout {
    /// # Safety
    /// `base` must be 4-byte aligned and valid for writes of the span
    /// `layout.validate()` returns, for as long as this value is used.
    pub unsafe fn new(base: *mut u8, layout: FramebufferLayout) -> Result<Self, String> {
        layout.validate()?;
        if base.is_null() || !(base as usize).is_multiple_of(4) {
            return Err("framebuffer mapping is null or misaligned".into());
        }
        Ok(Self {
            base,
            layout,
            tables: layout.tables(),
            band: vec![0; layout.width as usize * 4 * BAND_ROWS],
        })
    }
    pub fn layout(&self) -> FramebufferLayout {
        self.layout
    }
    fn check(&self, frame: &tiny_skia::Pixmap) -> Result<(), String> {
        if frame.width() != self.layout.width || frame.height() != self.layout.height {
            return Err("SFGOP frame dimensions changed".into());
        }
        Ok(())
    }
    /// The original path, unchanged apart from the two clock readings: convert
    /// the whole frame into a fresh buffer, then one volatile store per pixel.
    pub fn write_full_per_pixel(&mut self, frame: &tiny_skia::Pixmap) -> Result<PresentStats, String> {
        self.check(frame)?;
        let start = Instant::now();
        let native = self.layout.convert(frame.data())?;
        let converted = Instant::now();
        // WC mapped IOGOP scanout needs stores, not VirtIO selector 11 (gamma on IOGOP).
        // Only visible pixels are touched; preserve row padding. Mapping and user client
        // remain owned by this !Send/!Sync object until its Drop.
        for y in 0..self.layout.height as usize {
            for x in 0..self.layout.width as usize {
                let offset = y * self.layout.stride as usize + x * 4;
                let word = u32::from_le_bytes(native[offset..offset + 4].try_into().unwrap());
                // SAFETY: `offset` is inside a row of the validated mapping: x < width, so x * 4 + 4 <= stride, and y < height.
                unsafe { (self.base.add(offset) as *mut u32).write_volatile(word) };
            }
        }
        Ok(PresentStats {
            convert_us: (converted - start).as_micros() as u64,
            present_us: converted.elapsed().as_micros() as u64,
            bytes_written: u64::from(self.layout.width) * u64::from(self.layout.height) * 4,
            large_alloc_bytes: native.len() as u64,
        })
    }
    /// Convert the damaged rows a band at a time into one reused buffer and
    /// copy each converted row segment to the mapping as a block. Only visible
    /// pixels inside `damage` are written; row padding and everything else is
    /// left alone.
    pub fn write_damage(&mut self, frame: &tiny_skia::Pixmap, damage: &[DRect]) -> Result<PresentStats, String> {
        self.check(frame)?;
        let bounds = DRect::from_xywh(0, 0, self.layout.width, self.layout.height);
        let (width, stride) = (self.layout.width as usize, self.layout.stride as usize);
        let data = frame.data();
        let mut stats = PresentStats::default();
        let (mut convert_ns, mut present_ns) = (0u64, 0u64);
        for rect in damage {
            let r = rect.intersect(bounds);
            if r.is_empty() {
                continue;
            }
            let (x0, bytes) = (r.x0 as usize, r.width() as usize * 4);
            let mut y = r.y0 as usize;
            while y < r.y1 as usize {
                let rows = BAND_ROWS.min(r.y1 as usize - y);
                let start = Instant::now();
                for (i, native) in self.band.chunks_exact_mut(bytes).take(rows).enumerate() {
                    self.tables.pack_row(&data[((y + i) * width + x0) * 4..][..bytes], native);
                }
                let converted = Instant::now();
                for (i, native) in self.band.chunks_exact(bytes).take(rows).enumerate() {
                    // In bounds: y + i < height, x0 * 4 + bytes <= width * 4 <= stride,
                    // and stride * height is inside the validated mapping.
                    // SAFETY: in bounds as the comment above shows; the mapping outlives this call.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            native.as_ptr(),
                            self.base.add((y + i) * stride + x0 * 4),
                            bytes,
                        )
                    };
                }
                convert_ns += (converted - start).as_nanos() as u64;
                present_ns += converted.elapsed().as_nanos() as u64;
                y += rows;
            }
            stats.bytes_written += r.area() * 4;
        }
        stats.convert_us = convert_ns / 1000;
        stats.present_us = present_ns / 1000;
        Ok(stats)
    }
    /// Copy of the visible pixels as native words, row padding excluded.
    pub fn snapshot(&self) -> Vec<u8> {
        let (width, stride) = (self.layout.width as usize * 4, self.layout.stride as usize);
        let mut out = vec![0u8; width * self.layout.height as usize];
        for (y, row) in out.chunks_exact_mut(width).enumerate() {
            // SAFETY: each row starts at y * stride for y < height and copies `width` <= stride bytes, all inside the validated mapping; the snapshot buffer is separate memory.
            unsafe { std::ptr::copy_nonoverlapping(self.base.add(y * stride), row.as_mut_ptr(), width) };
        }
        out
    }
    /// Write back a `snapshot()` of this same layout.
    pub fn restore(&mut self, snapshot: &[u8]) -> Result<(), String> {
        let (width, stride) = (self.layout.width as usize * 4, self.layout.stride as usize);
        if snapshot.len() != width * self.layout.height as usize {
            return Err("framebuffer snapshot size mismatch".into());
        }
        for (y, row) in snapshot.chunks_exact(width).enumerate() {
            // SAFETY: each row starts at y * stride for y < height and copies `width` <= stride bytes, all inside the validated mapping; the snapshot buffer is separate memory.
            unsafe { std::ptr::copy_nonoverlapping(row.as_ptr(), self.base.add(y * stride), width) };
        }
        Ok(())
    }
}

/// Heap-backed framebuffer with a real SFGOP layout: the same conversion and
/// store code as the native presenter, but nothing is displayed. For host
/// tests and for separating compute cost from mapping cost in the benchmark.
pub struct MemoryFramebuffer {
    scanout: Scanout,
    _words: Vec<u32>,
    stats: Option<PresentStats>,
}
impl MemoryFramebuffer {
    /// 30-bit x2r10g10b10, the M1 panel layout.
    pub fn rgb30(width: u32, height: u32) -> Result<Self, String> {
        let stride = width.checked_mul(4).ok_or("width overflow")?;
        Self::with_layout(FramebufferLayout {
            width,
            height,
            stride,
            size: u64::from(stride) * u64::from(height),
            bpp: 32,
            pixel_type: 2,
            masks: [0x3ff00000, 0x000ffc00, 0x000003ff],
        })
    }
    pub fn with_layout(layout: FramebufferLayout) -> Result<Self, String> {
        let span = layout.validate()?;
        let mut words = vec![0u32; span.div_ceil(4)];
        // SAFETY: `words` holds at least the validated span, is 4-byte aligned, and is stored beside the scanout in this value, so it outlives it.
        let scanout = unsafe { Scanout::new(words.as_mut_ptr() as *mut u8, layout)? };
        Ok(Self { scanout, _words: words, stats: None })
    }
    pub fn size(&self) -> Size {
        Size::new(self.scanout.layout.width, self.scanout.layout.height)
    }
    pub fn scanout(&mut self) -> &mut Scanout {
        &mut self.scanout
    }
    /// The whole mapped span including row padding.
    pub fn bytes(&self) -> Vec<u8> {
        let span = self.scanout.layout.stride as usize * self.scanout.layout.height as usize;
        let mut out = vec![0u8; span];
        // SAFETY: the scanout's mapping is at least stride * height bytes (validated when it was built) and `out` is that long.
        unsafe { std::ptr::copy_nonoverlapping(self.scanout.base, out.as_mut_ptr(), span) };
        out
    }
}
impl Presenter for MemoryFramebuffer {
    fn present(&mut self, frame: &tiny_skia::Pixmap) -> Result<(), String> {
        self.stats = Some(self.scanout.write_full_per_pixel(frame)?);
        Ok(())
    }
    fn present_damage(&mut self, frame: &tiny_skia::Pixmap, damage: &[DRect]) -> Result<(), String> {
        self.stats = Some(self.scanout.write_damage(frame, damage)?);
        Ok(())
    }
    fn take_stats(&mut self) -> Option<PresentStats> {
        self.stats.take()
    }
}

// Layout matches SiliconForest src/Libraries/SFGOP/include/SFGOP.h (LP64).
#[repr(C)]
#[derive(Default)]
pub struct RawFramebuffer {
    pub master_port: u32,
    pub service: u32,
    pub connect: u32,
    pub address: u64,
    pub size: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub bpp: u32,
    pub pixel_type: u32,
    pub component_masks: [u32; 3],
}
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod native {
    use super::*;
    use std::marker::PhantomData;
    use std::rc::Rc;
    #[link(name = "SFGOP", kind = "static")]
    unsafe extern "C" {
        fn SFGOPOpen(fb: *mut RawFramebuffer) -> i32;
        fn SFGOPClose(fb: *mut RawFramebuffer);
    }
    // SiliconForest SFIOKitLib.h:92 / minimal IOKitLib.c:237. This is intentionally
    // NOT Apple's two-argument boolean-returning IOObjectConformsTo ABI.
    #[link(name = "IOKit", kind = "static")]
    unsafe extern "C" {
        fn IOObjectConformsTo(object: u32, class: *const libc::c_char, conforms: *mut i32) -> i32;
    }
    pub struct Pdgop {
        raw: RawFramebuffer,
        layout: FramebufferLayout,
        scanout: Scanout,
        stats: Option<PresentStats>,
        _single_thread: PhantomData<Rc<()>>,
    }
    impl Pdgop {
        pub fn open() -> Result<Self, String> {
            let mut raw = RawFramebuffer::default();
            // SAFETY: `raw` is a live, zeroed out-parameter of the structure the driver fills.
            let kr = unsafe { SFGOPOpen(&mut raw) };
            let layout = FramebufferLayout {
                width: raw.width,
                height: raw.height,
                stride: raw.stride,
                size: raw.size,
                bpp: raw.bpp,
                pixel_type: raw.pixel_type,
                masks: raw.component_masks,
            };
            let checked = (|| {
                if kr != 0 {
                    return Err(format!("SFGOPOpen failed: {kr:#x}"));
                }
                if raw.address == 0
                    || raw.address % 4 != 0
                    || raw.connect == 0
                    || raw.service == 0
                    || raw.address.checked_add(raw.size).is_none()
                {
                    return Err("SFGOP returned invalid mapping/connection".into());
                }
                layout.validate()?;
                let mut conforms = 0i32; // mach/arm/boolean.h: boolean_t is int
                // SAFETY: `raw.service` is the service handle SFGOPOpen returned, the class name is a NUL-terminated literal, and `conforms` is a live local.
                let kr = unsafe {
                    IOObjectConformsTo(raw.service, c"IOGOPFramebuffer".as_ptr(), &mut conforms)
                };
                if kr != 0 {
                    return Err(format!("IOObjectConformsTo failed: {kr:#x}"));
                }
                if conforms == 0 {
                    return Err("native presenter supports only IOGOPFramebuffer; VirtIO present protocol is distinct".into());
                }
                Ok(())
            })();
            // The mapping stays valid until SFGOPClose in Drop; Scanout holds
            // only the address and is dropped with this object.
            // SAFETY: the driver mapping stays valid until SFGOPClose in Drop, and `layout` was validated against it above.
            let scanout = checked.and_then(|()| unsafe { Scanout::new(raw.address as usize as *mut u8, layout) });
            let scanout = match scanout {
                Ok(scanout) => scanout,
                Err(error) => {
                    // SAFETY: `raw` was opened by SFGOPOpen above and is closed exactly once, here or in Drop, never both.
                    unsafe { SFGOPClose(&mut raw) };
                    return Err(error);
                }
            };
            Ok(Self {
                raw,
                layout,
                scanout,
                stats: None,
                _single_thread: PhantomData,
            })
        }
        pub fn layout(&self) -> FramebufferLayout {
            self.layout
        }
        /// Visible pixels currently in the mapping (native words, no padding).
        pub fn snapshot(&self) -> Vec<u8> {
            self.scanout.snapshot()
        }
        pub fn restore(&mut self, snapshot: &[u8]) -> Result<(), String> {
            self.scanout.restore(snapshot)?;
            fence();
            Ok(())
        }
        pub fn size(&self) -> Size {
            Size::new(self.layout.width, self.layout.height)
        }
    }
    fn fence() {
        // SAFETY: `dsb oshst` is a store barrier; it reads and writes no registers or memory Rust tracks.
        unsafe { std::arch::asm!("dsb oshst", options(nostack, preserves_flags)) };
    }
    impl Presenter for Pdgop {
        fn present(&mut self, frame: &tiny_skia::Pixmap) -> Result<(), String> {
            self.stats = Some(self.scanout.write_full_per_pixel(frame)?);
            fence();
            Ok(()) // store completion only: no vblank, panel, RPC or desktop claim
        }
        fn present_damage(&mut self, frame: &tiny_skia::Pixmap, damage: &[DRect]) -> Result<(), String> {
            let stats = self.scanout.write_damage(frame, damage)?;
            if stats.bytes_written != 0 {
                fence();
            }
            self.stats = Some(stats);
            Ok(()) // store completion only, as above
        }
        fn take_stats(&mut self) -> Option<PresentStats> {
            self.stats.take()
        }
    }
    impl Drop for Pdgop {
        fn drop(&mut self) {
            // SAFETY: `raw` was opened by SFGOPOpen above and is closed exactly once, here or in Drop, never both.
            unsafe { SFGOPClose(&mut self.raw) }
        }
    } // library unmaps before IOServiceClose
}
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub use native::Pdgop;
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub struct Pdgop;
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
impl Pdgop {
    pub fn open() -> Result<Self, String> {
        Err("native SFGOP requires aarch64-apple-darwin; no emulated native success".into())
    }
    pub fn size(&self) -> Size {
        unreachable!("Pdgop::open cannot succeed on this platform")
    }
    pub fn layout(&self) -> FramebufferLayout {
        unreachable!("Pdgop::open cannot succeed on this platform")
    }
    pub fn snapshot(&self) -> Vec<u8> {
        unreachable!("Pdgop::open cannot succeed on this platform")
    }
    pub fn restore(&mut self, _: &[u8]) -> Result<(), String> {
        Err("native SFGOP unavailable".into())
    }
}
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
impl Presenter for Pdgop {
    fn present(&mut self, _: &tiny_skia::Pixmap) -> Result<(), String> {
        Err("native SFGOP unavailable".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sfgop_lp64_layout() {
        assert_eq!(std::mem::size_of::<RawFramebuffer>(), 64);
        assert_eq!(std::mem::align_of::<RawFramebuffer>(), 8);
        assert_eq!(std::mem::offset_of!(RawFramebuffer, address), 16);
        assert_eq!(std::mem::offset_of!(RawFramebuffer, size), 24);
        assert_eq!(std::mem::offset_of!(RawFramebuffer, width), 32);
        assert_eq!(std::mem::offset_of!(RawFramebuffer, component_masks), 52);
    }

    fn frame(width: u32, height: u32, seed: u64) -> tiny_skia::Pixmap {
        let mut pix = tiny_skia::Pixmap::new(width, height).unwrap();
        let mut state = seed;
        for p in pix.data_mut().as_chunks_mut::<4>().0.iter_mut() {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            // Opaque, as every composited frame is.
            p.copy_from_slice(&[(state >> 56) as u8, (state >> 48) as u8, (state >> 40) as u8, 255]);
        }
        pix
    }
    fn padded(masks: [u32; 3]) -> FramebufferLayout {
        FramebufferLayout {
            width: 61,
            height: 23,
            stride: 61 * 4 + 16,
            size: (61 * 4 + 16) * 23,
            bpp: 32,
            pixel_type: 2,
            masks,
        }
    }

    #[test]
    fn full_damage_writes_the_same_bytes_as_the_per_pixel_path() {
        for masks in [[0x3ff00000, 0x000ffc00, 0x000003ff], [0x00ff0000, 0x0000ff00, 0x000000ff]] {
            let layout = padded(masks);
            let image = frame(61, 23, 11);
            let mut old = MemoryFramebuffer::with_layout(layout).unwrap();
            let mut new = MemoryFramebuffer::with_layout(layout).unwrap();
            old.present(&image).unwrap();
            new.present_damage(&image, &[DRect::new(0, 0, 61, 23)]).unwrap();
            assert_eq!(old.bytes(), new.bytes());
            assert_eq!(old.bytes(), layout.convert(image.data()).unwrap());
            let (a, b) = (old.take_stats().unwrap(), new.take_stats().unwrap());
            assert_eq!(a.bytes_written, 61 * 23 * 4);
            assert_eq!(b.bytes_written, 61 * 23 * 4);
            assert_eq!(a.large_alloc_bytes, layout.validate().unwrap() as u64);
            assert_eq!(b.large_alloc_bytes, 0);
        }
    }

    #[test]
    fn partial_damage_touches_only_damaged_visible_pixels() {
        let layout = padded([0x3ff00000, 0x000ffc00, 0x000003ff]);
        let (first, second) = (frame(61, 23, 1), frame(61, 23, 2));
        let mut fb = MemoryFramebuffer::with_layout(layout).unwrap();
        fb.present(&first).unwrap();
        let rects = [
            DRect::new(4, 2, 30, 9),
            DRect::new(50, 15, 400, 400), // clipped to the output
            DRect::new(-9, -9, 2, 2),
            DRect::new(100, 0, 200, 5), // outside
        ];
        fb.present_damage(&second, &rects).unwrap();
        assert_eq!(fb.take_stats().unwrap().bytes_written, (26 * 7 + 11 * 8 + 2 * 2) * 4);
        let (a, b) = (layout.convert(first.data()).unwrap(), layout.convert(second.data()).unwrap());
        let got = fb.bytes();
        let stride = layout.stride as usize;
        for y in 0..23usize {
            for x in 0..stride / 4 {
                let at = y * stride + x * 4;
                let inside = rects.iter().any(|r| r.contains_point(x as i32, y as i32)) && x < 61;
                let want = if inside { &b[at..at + 4] } else { &a[at..at + 4] };
                assert_eq!(&got[at..at + 4], want, "pixel {x},{y}");
            }
        }
        // Empty damage: nothing written at all.
        fb.present_damage(&second, &[]).unwrap();
        assert_eq!(fb.take_stats().unwrap(), PresentStats::default());
        assert_eq!(fb.bytes(), got);
    }

    #[test]
    fn snapshot_and_restore_round_trip_without_touching_padding() {
        let layout = padded([0x3ff00000, 0x000ffc00, 0x000003ff]);
        let mut fb = MemoryFramebuffer::with_layout(layout).unwrap();
        fb.present(&frame(61, 23, 5)).unwrap();
        let before = fb.bytes();
        let saved = fb.scanout().snapshot();
        fb.present(&frame(61, 23, 6)).unwrap();
        assert_ne!(fb.bytes(), before);
        fb.scanout().restore(&saved).unwrap();
        assert_eq!(fb.bytes(), before);
        assert!(fb.scanout().restore(&saved[1..]).is_err());
        assert!(fb.present(&frame(60, 23, 1)).is_err());
    }
}

//! Bounded owned pixels. Client memory is copied before wl_buffer.release.
use smithay::reexports::wayland_server::protocol::{wl_buffer::WlBuffer, wl_shm};
use smithay::wayland::shm::with_buffer_contents;
use wm_theme_api::DecorationBuffer;
use crate::damage::DRect;

pub const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;

pub fn image_len(width: u32, height: u32) -> Result<usize, String> {
    let len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .filter(|n| *n != 0 && *n <= MAX_IMAGE_BYTES)
        .ok_or("invalid or oversized image")?;
    Ok(len)
}

/// Checked placement of one ARGB8888/XRGB8888 image inside a client pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShmLayout {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub offset: usize,
    pub xrgb: bool,
}
impl ShmLayout {
    /// Every byte `offset + y * stride + x * 4 + 0..4` is inside `pool_len`.
    pub fn checked(
        width: i32,
        height: i32,
        stride: i32,
        offset: i32,
        format: wl_shm::Format,
        pool_len: usize,
    ) -> Result<Self, String> {
        let width = u32::try_from(width).map_err(|_| "negative width")?;
        let height = u32::try_from(height).map_err(|_| "negative height")?;
        image_len(width, height)?;
        let stride = usize::try_from(stride).map_err(|_| "negative stride")?;
        let offset = usize::try_from(offset).map_err(|_| "negative offset")?;
        let row = width as usize * 4;
        let end = stride
            .checked_mul(height as usize - 1)
            .and_then(|n| n.checked_add(row))
            .and_then(|n| n.checked_add(offset))
            .ok_or("SHM range overflow")?;
        if stride < row || end > pool_len {
            return Err("SHM range outside pool".into());
        }
        if !matches!(format, wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888) {
            return Err("only ARGB8888/XRGB8888 SHM is supported".into());
        }
        Ok(Self { width, height, stride, offset, xrgb: format == wl_shm::Format::Xrgb8888 })
    }
    pub fn bounds(self) -> DRect {
        DRect::from_xywh(0, 0, self.width, self.height)
    }
}

/// The original per-pixel copy: one fresh allocation and four volatile byte
/// reads per pixel. Kept as the reference and for the selectable legacy path.
///
/// # Safety
/// `ptr` must be readable for the pool length `layout` was checked against.
pub unsafe fn copy_shm_per_pixel(ptr: *const u8, layout: ShmLayout) -> DecorationBuffer {
    let ShmLayout { width, height, stride, offset, xrgb } = layout;
    let mut pixels = vec![0; width as usize * height as usize * 4];
    for y in 0..height as usize {
        for x in 0..width as usize {
            let source = offset + y * stride + x * 4;
            let dest = (y * width as usize + x) * 4;
            // No Rust references to mutable client memory; Smithay contains SIGBUS.
            // All offsets were checked above. Wayland SHM is native endian;
            // this backend supports little-endian Darwin ARM64 / host tests only.
            let mut bgra = [0u8; 4];
            for (i, byte) in bgra.iter_mut().enumerate() {
                // SAFETY: `source` and the bytes read stay inside the pool span `layout` was checked against; reads are byte copies, so no reference to client memory is formed.
                *byte = unsafe { ptr.add(source + i).read_volatile() };
            }
            let alpha = if xrgb { 255 } else { bgra[3] };
            // Malicious non-premultiplied data cannot violate tiny-skia's invariant.
            pixels[dest..dest + 4].copy_from_slice(&[
                bgra[2].min(alpha),
                bgra[1].min(alpha),
                bgra[0].min(alpha),
                alpha,
            ]);
        }
    }
    DecorationBuffer { width, height, pixels }
}

/// BGRA/BGRX (little-endian ARGB8888/XRGB8888) to clamped premultiplied RGBA,
/// in place on memory this process owns. Same arithmetic as the per-pixel copy.
pub fn shm_row_to_rgba(row: &mut [u8], xrgb: bool) {
    if xrgb {
        for p in row.as_chunks_mut::<4>().0.iter_mut() {
            p.swap(0, 2);
            p[3] = 255;
        }
    } else {
        for p in row.as_chunks_mut::<4>().0.iter_mut() {
            let alpha = p[3];
            let (b, g, r) = (p[0], p[1], p[2]);
            p[0] = r.min(alpha);
            p[1] = g.min(alpha);
            p[2] = b.min(alpha);
        }
    }
}

/// Row-wise copy of `rects` (image coordinates) from a client pool into the
/// matching rows of an owned RGBA image. Each row segment is first copied out
/// of client memory in one block and only then interpreted, so a client that
/// rewrites its pool mid-copy cannot defeat the premultiplication clamp.
/// Returns the number of pixels copied.
///
/// # Safety
/// `ptr` must be readable for the pool length `layout` was checked against.
pub unsafe fn copy_shm_rows(
    ptr: *const u8,
    layout: ShmLayout,
    dst: &mut [u8],
    rects: &[DRect],
) -> u64 {
    let width = layout.width as usize;
    assert_eq!(dst.len(), width * layout.height as usize * 4, "image size mismatch");
    let mut copied = 0;
    for rect in rects {
        let r = rect.intersect(layout.bounds());
        if r.is_empty() {
            continue;
        }
        let (x0, x1) = (r.x0 as usize, r.x1 as usize);
        let bytes = (x1 - x0) * 4;
        for y in r.y0 as usize..r.y1 as usize {
            let source = layout.offset + y * layout.stride + x0 * 4;
            let row = &mut dst[(y * width + x0) * 4..][..bytes];
            // No Rust reference to client memory is formed; Smithay contains SIGBUS.
            // SAFETY: `source` and the bytes read stay inside the pool span `layout` was checked against; reads are byte copies, so no reference to client memory is formed.
            unsafe { std::ptr::copy_nonoverlapping(ptr.add(source), row.as_mut_ptr(), bytes) };
            shm_row_to_rgba(row, layout.xrgb);
        }
        copied += r.area();
    }
    copied
}

/// Result of importing a client buffer with damage.
pub enum ShmCopy {
    /// The existing image was updated in place; these image-space rectangles
    /// (already clipped) are all that changed.
    Updated(Vec<DRect>),
    /// A complete new image (first commit, size change or no usable damage).
    New(DecorationBuffer),
}

fn with_layout<T>(
    buffer: &WlBuffer,
    f: impl FnOnce(*const u8, ShmLayout) -> Result<T, String>,
) -> Result<T, String> {
    with_buffer_contents(buffer, |ptr, pool_len, info| {
        let layout = ShmLayout::checked(
            info.width,
            info.height,
            info.stride,
            info.offset,
            info.format,
            pool_len,
        )?;
        f(ptr, layout)
    })
    .map_err(|e| format!("SHM access failed: {e:?}"))?
}

/// The size of an SHM buffer, validated like every import.
pub fn shm_size(buffer: &WlBuffer) -> Result<(u32, u32), String> {
    with_layout(buffer, |_, layout| Ok((layout.width, layout.height)))
}

/// Legacy import: always a new image, copied per pixel.
pub fn copy_shm(buffer: &WlBuffer) -> Result<DecorationBuffer, String> {
    // SAFETY: `ptr` and `layout` come from `with_layout`, which checked offset, stride and height against the client's pool size; the copy forms no Rust reference to client memory.
    with_layout(buffer, |ptr, layout| Ok(unsafe { copy_shm_per_pixel(ptr, layout) }))
}

/// Complete image, copied row-wise.
pub fn copy_shm_full(buffer: &WlBuffer) -> Result<DecorationBuffer, String> {
    with_layout(buffer, |ptr, layout| {
        let mut pixels = vec![0; layout.width as usize * layout.height as usize * 4];
        // SAFETY: `ptr` and `layout` come from `with_layout`, which checked offset, stride and height against the client's pool size; the copy forms no Rust reference to client memory.
        unsafe { copy_shm_rows(ptr, layout, &mut pixels, &[layout.bounds()]) };
        Ok(DecorationBuffer { width: layout.width, height: layout.height, pixels })
    })
}

/// Import with client damage. `accept(width, height)` runs after the buffer
/// is validated and before any pixel of `existing` is touched; its error is
/// returned unchanged. `existing` is updated in place only when it has the
/// buffer's size and the client reported damage; otherwise the result is a
/// complete new image (a client that reports no damage gets a full copy).
pub fn copy_shm_damaged(
    buffer: &WlBuffer,
    existing: Option<&mut DecorationBuffer>,
    damage: &[DRect],
    accept: impl FnOnce(u32, u32) -> Result<(), String>,
) -> Result<ShmCopy, String> {
    with_layout(buffer, |ptr, layout| {
        accept(layout.width, layout.height)?;
        if let Some(image) = existing.filter(|i| {
            !damage.is_empty() && i.width == layout.width && i.height == layout.height
        }) {
            // Clip and merge first: overlapping client rectangles are common.
            let mut region = crate::damage::Region::new(layout.width, layout.height);
            for r in damage {
                region.add(*r);
            }
            // SAFETY: `ptr` and `layout` come from `with_layout`, which checked offset, stride and height against the client's pool size; the copy forms no Rust reference to client memory.
            unsafe { copy_shm_rows(ptr, layout, &mut image.pixels, region.rects()) };
            return Ok(ShmCopy::Updated(region.rects().to_vec()));
        }
        let mut pixels = vec![0; layout.width as usize * layout.height as usize * 4];
        // SAFETY: `ptr` and `layout` come from `with_layout`, which checked offset, stride and height against the client's pool size; the copy forms no Rust reference to client memory.
        unsafe { copy_shm_rows(ptr, layout, &mut pixels, &[layout.bounds()]) };
        Ok(ShmCopy::New(DecorationBuffer { width: layout.width, height: layout.height, pixels }))
    })
}

/// Validated 32-bit direct framebuffer layout, with either 8- or 10-bit RGB.
#[derive(Clone, Copy, Debug)]
pub struct FramebufferLayout {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub size: u64,
    pub bpp: u32,
    pub pixel_type: u32,
    pub masks: [u32; 3],
}
impl FramebufferLayout {
    pub fn validate(self) -> Result<usize, String> {
        image_len(self.width, self.height)?;
        if self.bpp != 32 || self.pixel_type != 2 {
            return Err("SFGOP requires 32-bit RGB direct pixels".into());
        }
        if self.masks != [0x00ff0000, 0x0000ff00, 0x000000ff]
            && self.masks != [0x3ff00000, 0x000ffc00, 0x000003ff]
        {
            return Err("unsupported SFGOP RGB masks".into());
        }
        let span = u64::from(self.stride)
            .checked_mul(u64::from(self.height))
            .ok_or("framebuffer span overflow")?;
        if u64::from(self.stride) < u64::from(self.width) * 4
            || !self.stride.is_multiple_of(4)
            || span > self.size
            || span > MAX_IMAGE_BYTES as u64
        {
            return Err("framebuffer layout exceeds mapped span".into());
        }
        Ok(span as usize)
    }
    pub fn pack_rgb(self, rgba: &[u8]) -> u32 {
        self.masks
            .iter()
            .zip(rgba)
            .fold(0, |word, (mask, channel)| {
                let shift = mask.trailing_zeros();
                let max = mask >> shift;
                word | (((u32::from(*channel) * max + 127) / 255) << shift)
            })
    }
    /// Per-channel lookup tables holding exactly `pack_rgb`'s arithmetic.
    pub fn tables(self) -> PackTables {
        let mut tables = [[0u32; 256]; 3];
        for (table, mask) in tables.iter_mut().zip(self.masks) {
            let shift = mask.trailing_zeros();
            let max = mask >> shift;
            for (value, slot) in table.iter_mut().enumerate() {
                *slot = ((value as u32 * max + 127) / 255) << shift;
            }
        }
        PackTables(tables)
    }
    pub fn convert(self, rgba: &[u8]) -> Result<Vec<u8>, String> {
        let span = self.validate()?;
        if rgba.len() != image_len(self.width, self.height)? {
            return Err("frame size mismatch".into());
        }
        let mut native = vec![0; span];
        for y in 0..self.height as usize {
            for x in 0..self.width as usize {
                let src = (y * self.width as usize + x) * 4;
                let dst = y * self.stride as usize + x * 4;
                native[dst..dst + 4]
                    .copy_from_slice(&self.pack_rgb(&rgba[src..src + 4]).to_le_bytes());
            }
        }
        Ok(native)
    }
}

/// Row converter from RGBA bytes to native framebuffer words (little endian).
#[derive(Clone)]
pub struct PackTables([[u32; 256]; 3]);
impl PackTables {
    #[inline]
    pub fn pack(&self, rgba: &[u8]) -> u32 {
        self.0[0][rgba[0] as usize] | self.0[1][rgba[1] as usize] | self.0[2][rgba[2] as usize]
    }
    /// `native.len() == rgba.len()`; both hold whole 4-byte pixels.
    pub fn pack_row(&self, rgba: &[u8], native: &mut [u8]) {
        assert_eq!(rgba.len(), native.len());
        for (out, pixel) in native.as_chunks_mut::<4>().0.iter_mut().zip(rgba.as_chunks::<4>().0.iter()) {
            out.copy_from_slice(&self.pack(pixel).to_le_bytes());
        }
    }
}

/// FNV-1a over a byte slice: a cheap, stable fingerprint for comparing two
/// captures of the same region in log lines. Not a cryptographic hash.
pub fn fingerprint(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for b in bytes {
        hash = (hash ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// An image at another size: how a buffer that was not rendered at the
/// display scale is shown at the size its surface has on the panel. A whole
/// multiple is repeated pixel for pixel (sharp, blocky); anything else is
/// filtered. Clients that render at the display scale never come here.
pub fn resample(image: &DecorationBuffer, width: u32, height: u32) -> Result<DecorationBuffer, String> {
    image_len(width, height)?;
    if image.width == 0 || image.height == 0 || image.pixels.len() != image.width as usize * image.height as usize * 4 {
        return Err("image to resample is empty or truncated".into());
    }
    if (image.width, image.height) == (width, height) {
        return Ok(image.clone());
    }
    let source = tiny_skia::PixmapRef::from_bytes(&image.pixels, image.width, image.height).ok_or("image to resample is invalid")?;
    let mut target = tiny_skia::Pixmap::new(width, height).ok_or("resampled image allocation failed")?;
    let whole = width.is_multiple_of(image.width) && height.is_multiple_of(image.height) && width / image.width == height / image.height;
    let paint = tiny_skia::PixmapPaint {
        quality: if whole { tiny_skia::FilterQuality::Nearest } else { tiny_skia::FilterQuality::Bilinear },
        blend_mode: tiny_skia::BlendMode::Source,
        ..Default::default()
    };
    let transform = tiny_skia::Transform::from_scale(width as f32 / image.width as f32, height as f32 / image.height as f32);
    target.draw_pixmap(0, 0, source, &paint, transform, None);
    Ok(DecorationBuffer { width, height, pixels: target.take() })
}


#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rgb30_stride_and_bounds() {
        let l = FramebufferLayout {
            width: 2,
            height: 1,
            stride: 12,
            size: 12,
            bpp: 32,
            pixel_type: 2,
            masks: [0x3ff00000, 0xffc00, 0x3ff],
        };
        let data = l.convert(&[255, 0, 0, 255, 0, 128, 255, 255]).unwrap();
        assert_eq!(
            u32::from_le_bytes(data[..4].try_into().unwrap()),
            0x3ff00000
        );
        assert_eq!(
            u32::from_le_bytes(data[4..8].try_into().unwrap()),
            (514 << 10) | 1023
        );
        assert_eq!(&data[8..], &[0; 4]);
        assert!(FramebufferLayout { size: 11, ..l }.validate().is_err());
        assert!(FramebufferLayout { stride: 7, ..l }.validate().is_err());
        assert!(FramebufferLayout { masks: [0; 3], ..l }.validate().is_err());
        assert!(image_len(u32::MAX, u32::MAX).is_err());
    }

    const RGB30: [u32; 3] = [0x3ff00000, 0x000ffc00, 0x000003ff];
    const RGB24: [u32; 3] = [0x00ff0000, 0x0000ff00, 0x000000ff];
    fn layout(width: u32, height: u32, stride: u32, masks: [u32; 3]) -> FramebufferLayout {
        FramebufferLayout {
            width,
            height,
            stride,
            size: u64::from(stride) * u64::from(height),
            bpp: 32,
            pixel_type: 2,
            masks,
        }
    }
    fn noise(len: usize, mut seed: u64) -> Vec<u8> {
        (0..len)
            .map(|_| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (seed >> 56) as u8
            })
            .collect()
    }

    #[test]
    fn tables_equal_pack_rgb_for_every_channel_value() {
        for masks in [RGB30, RGB24] {
            let l = layout(1, 1, 4, masks);
            let tables = l.tables();
            for channel in 0..3 {
                for value in 0..=255u8 {
                    let mut rgba = [0u8, 0, 0, 255];
                    rgba[channel] = value;
                    assert_eq!(tables.pack(&rgba), l.pack_rgb(&rgba));
                }
            }
            // Alpha never reaches the framebuffer and x2 padding stays zero.
            for rgba in noise(4 * 4096, 7).as_chunks::<4>().0.iter() {
                assert_eq!(tables.pack(rgba), l.pack_rgb(rgba));
                assert_eq!(tables.pack(rgba) & !(masks[0] | masks[1] | masks[2]), 0);
            }
            assert_eq!(tables.pack(&[255, 255, 255, 0]), masks[0] | masks[1] | masks[2]);
        }
    }

    #[test]
    fn row_conversion_matches_whole_frame_convert_with_row_padding() {
        for masks in [RGB30, RGB24] {
            // Stride wider than the visible row: padding must stay untouched.
            let l = layout(37, 11, 37 * 4 + 12, masks);
            let rgba = noise(37 * 11 * 4, 99);
            let reference = l.convert(&rgba).unwrap();
            let tables = l.tables();
            let mut native = vec![0u8; reference.len()];
            for y in 0..11usize {
                let src = &rgba[y * 37 * 4..][..37 * 4];
                tables.pack_row(src, &mut native[y * l.stride as usize..][..37 * 4]);
            }
            assert_eq!(native, reference);
            // Any sub-rectangle converted alone equals the same bytes of the frame.
            let (x0, x1, y0, y1) = (5usize, 30usize, 2usize, 9usize);
            let mut partial = vec![0xa5u8; reference.len()];
            for y in y0..y1 {
                let src = &rgba[(y * 37 + x0) * 4..(y * 37 + x1) * 4];
                let at = y * l.stride as usize + x0 * 4;
                tables.pack_row(src, &mut partial[at..at + (x1 - x0) * 4]);
            }
            for y in 0..11usize {
                for x in 0..(l.stride as usize / 4) {
                    let at = y * l.stride as usize + x * 4;
                    let inside = (x0..x1).contains(&x) && (y0..y1).contains(&y);
                    let want = if inside { &reference[at..at + 4] } else { &[0xa5; 4][..] };
                    assert_eq!(&partial[at..at + 4], want);
                }
            }
        }
    }

    fn shm(width: i32, height: i32, stride: i32, offset: i32, xrgb: bool, pool: usize) -> ShmLayout {
        let format = if xrgb { wl_shm::Format::Xrgb8888 } else { wl_shm::Format::Argb8888 };
        ShmLayout::checked(width, height, stride, offset, format, pool).unwrap()
    }

    #[test]
    fn shm_layout_keeps_the_original_refusals() {
        let ok = |w, h, s, o, len| {
            ShmLayout::checked(w, h, s, o, wl_shm::Format::Argb8888, len).is_ok()
        };
        assert!(ok(4, 2, 16, 0, 32));
        assert!(ok(4, 2, 20, 8, 8 + 20 + 16));
        assert!(!ok(4, 2, 20, 8, 8 + 20 + 15));
        assert!(!ok(4, 2, 12, 0, 64)); // stride narrower than a row
        assert!(!ok(-1, 2, 16, 0, 64) && !ok(4, 0, 16, 0, 64) && !ok(4, 2, -16, 0, 64));
        assert!(!ok(4, 2, 16, -4, 64));
        assert!(ShmLayout::checked(4, 2, 16, 0, wl_shm::Format::Rgb565, 64).is_err());
    }

    #[test]
    fn row_copy_equals_per_pixel_copy_including_hostile_alpha() {
        for xrgb in [false, true] {
            let (w, h, stride, offset) = (23, 9, 23 * 4 + 8, 12);
            let pool = noise(offset + stride * h, if xrgb { 3 } else { 4 });
            let l = shm(w as i32, h as i32, stride as i32, offset as i32, xrgb, pool.len());
            // SAFETY: the test pool is a live local at least as large as `l`, which `ShmLayout::checked` accepted for that pool size.
            let reference = unsafe { copy_shm_per_pixel(pool.as_ptr(), l) };
            let mut rows = vec![0u8; w * h * 4];
            // SAFETY: the test pool is a live local at least as large as `l`, which `ShmLayout::checked` accepted for that pool size.
            let n = unsafe { copy_shm_rows(pool.as_ptr(), l, &mut rows, &[l.bounds()]) };
            assert_eq!(n, (w * h) as u64);
            assert_eq!(rows, reference.pixels);
            // Premultiplied invariant holds for arbitrary client bytes.
            assert!(rows.as_chunks::<4>().0.iter().all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]));
            assert!(!xrgb || rows.as_chunks::<4>().0.iter().all(|p| p[3] == 255));
        }
    }

    #[test]
    fn damaged_copy_updates_only_reported_rectangles() {
        let (w, h) = (40usize, 30usize);
        let before = noise(w * h * 4, 1);
        let after = noise(w * h * 4, 2);
        let l = shm(w as i32, h as i32, (w * 4) as i32, 0, true, after.len());
        // SAFETY: the test pool is a live local at least as large as `l`, which `ShmLayout::checked` accepted for that pool size.
        let old = unsafe { copy_shm_per_pixel(before.as_ptr(), l) };
        // SAFETY: the test pool is a live local at least as large as `l`, which `ShmLayout::checked` accepted for that pool size.
        let new = unsafe { copy_shm_per_pixel(after.as_ptr(), l) };
        let rects = [
            DRect::new(3, 4, 17, 9),
            DRect::new(-5, 20, 8, 400), // clipped to the image
            DRect::new(100, 100, 200, 200), // entirely outside: ignored
        ];
        let mut image = old.pixels.clone();
        // SAFETY: the test pool is a live local at least as large as `l`, which `ShmLayout::checked` accepted for that pool size.
        let n = unsafe { copy_shm_rows(after.as_ptr(), l, &mut image, &rects) };
        assert_eq!(n, 14 * 5 + 8 * 10);
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let at = (y as usize * w + x as usize) * 4;
                let inside = rects.iter().any(|r| r.contains_point(x, y));
                let want = if inside { &new.pixels[at..at + 4] } else { &old.pixels[at..at + 4] };
                assert_eq!(&image[at..at + 4], want, "pixel {x},{y}");
            }
        }
        // Full damage reproduces the per-pixel image exactly.
        // SAFETY: the test pool is a live local at least as large as `l`, which `ShmLayout::checked` accepted for that pool size.
        unsafe { copy_shm_rows(after.as_ptr(), l, &mut image, &[l.bounds()]) };
        assert_eq!(image, new.pixels);
    }
}

//! Pixel-format conversion (portable; compiled and tested on all platforms).
//!
//! ScreenCaptureKit hands us NV12 directly, but Windows Graphics Capture delivers BGRA.
//! Hardware H.264 encoders want NV12 (4:2:0), so we convert BGRA → NV12 on the CPU here.
//! BT.601 limited-range coefficients (the common default for sub-4K H.264 / MPEG-TS).

use crate::frame::Nv12Planes;

/// Convert a BGRA buffer to NV12 planes.
///
/// `src` is `height` rows of `src_stride` bytes; only the first `width * 4` bytes of each
/// row are pixels (the rest is alignment padding). Output planes are tightly packed: the Y
/// plane has `y_stride == width`, the interleaved UV plane `uv_stride == round_up_even(width)`.
/// Odd `width`/`height` are rounded down to even so 4:2:0 subsampling stays in bounds.
#[must_use]
pub fn bgra_to_nv12(src: &[u8], width: u32, height: u32, src_stride: u32) -> Nv12Planes {
    // 4:2:0 needs even dimensions; round down so every 2×2 block is fully present.
    let w = (width & !1) as usize;
    let h = (height & !1) as usize;
    let stride = src_stride as usize;

    let mut y_plane = vec![0u8; w * h];
    let mut uv_plane = vec![0u8; w * (h / 2)]; // (w/2 * h/2) UV pairs = w*h/4 * 2 = w*h/2

    for row in 0..h {
        let src_row = row * stride;
        let y_row = row * w;
        for col in 0..w {
            let i = src_row + col * 4;
            let (b, g, r) = (src[i] as i32, src[i + 1] as i32, src[i + 2] as i32);
            y_plane[y_row + col] = clamp_u8(((66 * r + 129 * g + 25 * b + 128) >> 8) + 16);
        }
    }

    // Chroma: average each 2×2 block, then BT.601 U/V.
    let uv_w = w / 2;
    for by in 0..(h / 2) {
        for bx in 0..uv_w {
            let (mut rs, mut gs, mut bs) = (0i32, 0i32, 0i32);
            for dy in 0..2 {
                for dx in 0..2 {
                    let px = (by * 2 + dy) * stride + (bx * 2 + dx) * 4;
                    bs += src[px] as i32;
                    gs += src[px + 1] as i32;
                    rs += src[px + 2] as i32;
                }
            }
            let (r, g, b) = (rs / 4, gs / 4, bs / 4);
            let u = clamp_u8(((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128);
            let v = clamp_u8(((112 * r - 94 * g - 18 * b + 128) >> 8) + 128);
            let o = by * w + bx * 2;
            uv_plane[o] = u;
            uv_plane[o + 1] = v;
        }
    }

    Nv12Planes {
        y: y_plane,
        y_stride: w as u32,
        uv: uv_plane,
        uv_stride: w as u32,
    }
}

#[inline]
fn clamp_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `width`×`height` BGRA buffer (tightly packed) filled with one color.
    fn solid_bgra(width: u32, height: u32, r: u8, g: u8, b: u8) -> Vec<u8> {
        let mut v = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..(width * height) {
            v.extend_from_slice(&[b, g, r, 255]);
        }
        v
    }

    #[test]
    fn plane_sizes_are_correct() {
        let src = solid_bgra(4, 4, 10, 20, 30);
        let nv12 = bgra_to_nv12(&src, 4, 4, 16);
        assert_eq!(nv12.y.len(), 16); // 4*4
        assert_eq!(nv12.uv.len(), 8); // 4*2
        assert_eq!(nv12.y_stride, 4);
        assert_eq!(nv12.uv_stride, 4);
    }

    #[test]
    fn pure_black_and_white_map_to_limited_range() {
        // Limited-range Y: black → 16, white → 235.
        let black = solid_bgra(2, 2, 0, 0, 0);
        let white = solid_bgra(2, 2, 255, 255, 255);
        assert_eq!(bgra_to_nv12(&black, 2, 2, 8).y[0], 16);
        assert_eq!(bgra_to_nv12(&white, 2, 2, 8).y[0], 235);
    }

    #[test]
    fn grey_is_neutral_chroma() {
        // A neutral grey should give U≈V≈128 (no color).
        let grey = solid_bgra(2, 2, 128, 128, 128);
        let nv12 = bgra_to_nv12(&grey, 2, 2, 8);
        assert!((nv12.uv[0] as i32 - 128).abs() <= 1, "U near 128");
        assert!((nv12.uv[1] as i32 - 128).abs() <= 1, "V near 128");
    }

    #[test]
    fn red_pushes_v_high() {
        // Saturated red → large positive V (Cr), U (Cb) below neutral.
        let red = solid_bgra(2, 2, 255, 0, 0);
        let nv12 = bgra_to_nv12(&red, 2, 2, 8);
        assert!(
            nv12.uv[1] > 200,
            "V should be high for red, got {}",
            nv12.uv[1]
        );
        assert!(nv12.uv[0] < 128, "U should dip for red, got {}", nv12.uv[0]);
    }

    #[test]
    fn odd_dimensions_round_down_to_even() {
        // 3×3 rounds to 2×2: Y = 4, UV = 2.
        let src = solid_bgra(3, 3, 50, 60, 70);
        let nv12 = bgra_to_nv12(&src, 3, 3, 12);
        assert_eq!(nv12.y.len(), 4);
        assert_eq!(nv12.uv.len(), 2);
    }

    #[test]
    fn honors_row_stride_padding() {
        // 2×2 image stored with a padded stride of 12 bytes/row (8 pixels + 4 pad).
        let mut src = Vec::new();
        for _ in 0..2 {
            src.extend_from_slice(&[0, 0, 255, 255, 0, 0, 255, 255]); // two red pixels
            src.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]); // padding (must be ignored)
        }
        let nv12 = bgra_to_nv12(&src, 2, 2, 12);
        // All pixels red → Y the same everywhere, padding ignored.
        assert!(nv12.y.iter().all(|&y| y == nv12.y[0]));
        assert!(nv12.uv[1] > 200, "V high for red despite padding");
    }
}

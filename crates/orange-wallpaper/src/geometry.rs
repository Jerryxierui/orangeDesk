//! 渲染几何纯逻辑（跨平台可测，无 Windows 依赖）。

/// 目标区域尺寸
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Size {
    pub w: f32,
    pub h: f32,
}

impl Size {
    pub fn new(w: f32, h: f32) -> Self {
        Self { w, h }
    }
}

/// cover 模式：等比缩放填满目标区域、居中裁剪超出部分（壁纸默认观感）。
///
/// 返回绘制矩形 `(x, y, w, h)`（目标坐标系，允许负偏移——超出屏幕的部分
/// 被交换链自动裁剪）。任一尺寸非正返回 `None`，调用方降级纯色填充。
pub fn cover_rect(dst: Size, src: Size) -> Option<(f32, f32, f32, f32)> {
    if dst.w <= 0.0 || dst.h <= 0.0 || src.w <= 0.0 || src.h <= 0.0 {
        return None;
    }
    let scale = (dst.w / src.w).max(dst.h / src.h);
    let w = src.w * scale;
    let h = src.h * scale;
    let x = (dst.w - w) / 2.0;
    let y = (dst.h - h) / 2.0;
    Some((x, y, w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_ratio_fills_exactly() {
        let rect = cover_rect(Size::new(1920.0, 1080.0), Size::new(3840.0, 2160.0)).unwrap();
        assert_eq!(rect, (0.0, 0.0, 1920.0, 1080.0));
    }

    #[test]
    fn wide_image_on_tall_screen_crops_sides() {
        // 16:9 图 → 9:16 竖屏：高度铺满（h == 屏高），宽度溢出左右裁剪（x < 0）
        let (x, y, w, h) =
            cover_rect(Size::new(1080.0, 1920.0), Size::new(3840.0, 2160.0)).unwrap();
        assert!(
            (h - 1920.0).abs() < 0.01,
            "height must cover screen exactly"
        );
        assert!(w > 1080.0, "width overflows and is cropped");
        assert!((y - 0.0).abs() < 0.01, "no vertical crop");
        assert!(
            (x - (1080.0 - w) / 2.0).abs() < 0.01,
            "horizontally centered"
        );
    }

    #[test]
    fn tall_image_on_wide_screen_crops_top_bottom() {
        // 竖图 → 横屏：宽度铺满（w == 屏宽），高度溢出上下裁剪（y < 0）
        let (x, y, w, h) =
            cover_rect(Size::new(1920.0, 1080.0), Size::new(1080.0, 2160.0)).unwrap();
        assert!((w - 1920.0).abs() < 0.01, "width must cover screen exactly");
        assert!(h > 1080.0, "height overflows and is cropped");
        assert!((x - 0.0).abs() < 0.01, "no horizontal crop");
        assert!((y - (1080.0 - h) / 2.0).abs() < 0.01, "vertically centered");
    }

    #[test]
    fn degenerate_sizes_return_none() {
        assert!(cover_rect(Size::new(0.0, 100.0), Size::new(10.0, 10.0)).is_none());
        assert!(cover_rect(Size::new(100.0, 100.0), Size::new(0.0, 10.0)).is_none());
        assert!(cover_rect(Size::new(-1.0, 100.0), Size::new(10.0, 10.0)).is_none());
    }
}

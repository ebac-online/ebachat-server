//! Images of uploaded files and linked pictures: sizes, EXIF orientation, the thumbnail, preview and mini preview
//! Mattermost makes, SVG sizes and GIF frame counts.

// Based on Mattermost server/channels/app/imaging (preview.go, orientation.go, svg.go), app/file.go and
// utils/imgutils (gif.go), Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::io::Cursor;

use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
use image::imageops::FilterType;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};

pub const THUMBNAIL_WIDTH: u32 = 120;
pub const THUMBNAIL_HEIGHT: u32 = 100;
pub const PREVIEW_WIDTH: u32 = 1920;
pub const MINI_PREVIEW: u32 = 16;
pub const JPEG_QUALITY: u8 = 90;

/// Go's `image` format name.
pub fn format_name(f: ImageFormat) -> &'static str {
    match f {
        ImageFormat::Jpeg => "jpeg",
        ImageFormat::Png => "png",
        ImageFormat::Gif => "gif",
        ImageFormat::Bmp => "bmp",
        ImageFormat::Tiff => "tiff",
        ImageFormat::WebP => "webp",
        _ => "",
    }
}

/// What `DecodeConfig` and `GetImageOrientation` tell of an image without decoding it: format, size as stored,
/// and whether EXIF says it is turned a quarter (so width and height swap upright).
pub struct Header {
    pub format: ImageFormat,
    pub width: u32,
    pub height: u32,
    pub quarter_turn: bool,
}

fn reader(data: &[u8]) -> Option<ImageReader<Cursor<&[u8]>>> {
    let r = ImageReader::new(Cursor::new(data)).with_guessed_format().ok()?;
    // Only the formats Go's server registers.
    matches!(
        r.format()?,
        ImageFormat::Jpeg
            | ImageFormat::Png
            | ImageFormat::Gif
            | ImageFormat::Bmp
            | ImageFormat::Tiff
            | ImageFormat::WebP
    )
    .then_some(r)
}

pub fn header(data: &[u8]) -> Option<Header> {
    let r = reader(data)?;
    let format = r.format()?;
    let mut d = r.into_decoder().ok()?;
    let (width, height) = d.dimensions();
    let quarter_turn = matches!(
        d.orientation().unwrap_or(Orientation::NoTransforms),
        Orientation::Rotate90 | Orientation::Rotate270 | Orientation::Rotate90FlipH | Orientation::Rotate270FlipH
    );
    Some(Header { format, width, height, quarter_turn })
}

/// The image decoded and turned upright (`MakeImageUpright`); a GIF's first frame.
pub fn decode_upright(data: &[u8]) -> Option<(DynamicImage, ImageFormat)> {
    let r = reader(data)?;
    let format = r.format()?;
    let mut d = r.into_decoder().ok()?;
    let o = d.orientation().unwrap_or(Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(d).ok()?;
    img.apply_orientation(o);
    Some((img, format))
}

/// `imaging.Resize` with one side 0: the other kept in proportion, at least 1, rounded.
fn resize(img: &DynamicImage, w: u32, h: u32) -> DynamicImage {
    let (sw, sh) = (img.width().max(1) as f64, img.height().max(1) as f64);
    let (w, h) = match (w, h) {
        (0, h) => (((h as f64 * sw / sh + 0.5).floor()).max(1.0) as u32, h),
        (w, 0) => (w, ((w as f64 * sh / sw + 0.5).floor()).max(1.0) as u32),
        wh => wh,
    };
    if (w, h) == (img.width(), img.height()) {
        return img.clone();
    }
    img.resize_exact(w, h, FilterType::Lanczos3)
}

/// `GenerateThumbnail`: 120 wide for a landscape image, 100 high otherwise (small ones grow too, as in Go).
pub fn thumbnail(img: &DynamicImage) -> DynamicImage {
    if img.width() > img.height() { resize(img, THUMBNAIL_WIDTH, 0) } else { resize(img, 0, THUMBNAIL_HEIGHT) }
}

/// `GeneratePreview`: at most 1920 wide.
pub fn preview(img: &DynamicImage) -> DynamicImage {
    if img.width() > PREVIEW_WIDTH { resize(img, PREVIEW_WIDTH, 0) } else { img.clone() }
}

/// JPEG at quality 90 with 4:2:0 chroma, as Go's `image/jpeg` writes it.
pub fn jpeg(img: &DynamicImage) -> Option<Vec<u8>> {
    let rgb = img.to_rgb8();
    let (w, h) = (u16::try_from(rgb.width()).ok()?, u16::try_from(rgb.height()).ok()?);
    let mut out = vec![];
    let mut e = jpeg_encoder::Encoder::new(&mut out, JPEG_QUALITY);
    e.set_sampling_factor(jpeg_encoder::SamplingFactor::R_4_2_0);
    e.encode(rgb.as_raw(), w, h, jpeg_encoder::ColorType::Rgb).ok()?;
    Some(out)
}

pub fn png(img: &DynamicImage) -> Option<Vec<u8>> {
    let mut out = vec![];
    img.write_with_encoder(PngEncoder::new_with_quality(&mut out, CompressionType::Best, PngFilter::Adaptive)).ok()?;
    Some(out)
}

/// `GenerateMiniPreviewImage`: 16×16 JPEG.
pub fn mini_preview(img: &DynamicImage) -> Option<Vec<u8>> {
    jpeg(&img.resize_exact(MINI_PREVIEW, MINI_PREVIEW, FilterType::Lanczos3))
}

/// `imaging.ParseSVG`: the root element's `width`/`height`, else its `viewBox`.
pub fn svg_size(data: &[u8]) -> Option<(u32, u32)> {
    let text = std::str::from_utf8(&data[..data.len().min(64 * 1024)]).ok()?;
    // The first start tag that is not a declaration, comment or processing instruction.
    let mut rest = text;
    let tag = loop {
        let i = rest.find('<')?;
        rest = &rest[i + 1..];
        if rest.starts_with('?') || rest.starts_with('!') {
            continue;
        }
        break &rest[..rest.find('>')?];
    };
    let attr = |name: &str| attr_value(tag, name);
    let num = |v: &str| -> Option<u32> {
        let n: f64 = v.trim().to_lowercase().trim_end_matches("px").parse().ok()?;
        (n.is_finite() && (1.0..=i32::MAX as f64).contains(&n)).then(|| n.round() as u32)
    };
    if let (Some(w), Some(h)) = (attr("width").and_then(|v| num(&v)), attr("height").and_then(|v| num(&v))) {
        return Some((w, h));
    }
    let vb = attr("viewBox")?;
    let parts: Vec<&str> = vb.split(|c: char| c == ',' || c.is_whitespace()).filter(|s| !s.is_empty()).collect();
    if parts.len() != 4 || parts[..2].iter().any(|p| !p.parse::<f64>().is_ok_and(f64::is_finite)) {
        return None;
    }
    Some((num(parts[2])?, num(parts[3])?))
}

/// An attribute of a tag's text (`name="v"` or `name='v'`), by local name.
fn attr_value(tag: &str, name: &str) -> Option<String> {
    let b = tag.as_bytes();
    let mut i = 0;
    while let Some(p) = tag[i..].find(name) {
        let at = i + p;
        i = at + name.len();
        let before_ok = at == 0 || b[at - 1].is_ascii_whitespace() || b[at - 1] == b':';
        let mut j = at + name.len();
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if !before_ok || j >= b.len() || b[j] != b'=' {
            continue;
        }
        j += 1;
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        let q = *b.get(j)?;
        if q == b'"' || q == b'\'' {
            let end = tag[j + 1..].find(q as char)?;
            return Some(tag[j + 1..j + 1 + end].to_string());
        }
        let end = tag[j..].find(|c: char| c.is_whitespace() || c == '/').unwrap_or(tag.len() - j);
        return Some(tag[j..j + end].to_string());
    }
    None
}

/// `imgutils.CountGIFFrames`: image descriptors in the block stream, without decoding.
pub fn gif_frames(data: &[u8]) -> Option<u32> {
    if data.len() < 13 || !data.starts_with(b"GIF8") {
        return None;
    }
    let mut i = 13;
    let flags = data[10];
    if flags & 0x80 != 0 {
        i += 3 << ((flags & 7) + 1);
    }
    let skip_sub_blocks = |mut i: usize| -> Option<usize> {
        loop {
            let n = *data.get(i)? as usize;
            i += 1 + n;
            if n == 0 {
                return Some(i);
            }
        }
    };
    let mut frames = 0;
    loop {
        match *data.get(i)? {
            0x2C => {
                frames += 1;
                let f = *data.get(i + 9)?;
                i += 10;
                if f & 0x80 != 0 {
                    i += 3 << ((f & 7) + 1);
                }
                i = skip_sub_blocks(i + 1)?;
            }
            0x21 => i = skip_sub_blocks(i + 2)?,
            0x3B => return Some(frames),
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn svg_sizes() {
        assert_eq!(svg_size(br#"<?xml version="1.0"?><svg width="20px" height="10" xmlns="x"/>"#), Some((20, 10)));
        assert_eq!(svg_size(br#"<svg viewBox="0 0 30.4 40"></svg>"#), Some((30, 40)));
        assert_eq!(svg_size(br#"<svg width="100%" viewBox="0,0,5,6"></svg>"#), Some((5, 6)));
        assert_eq!(svg_size(br#"<svg></svg>"#), None);
    }

    #[test]
    fn thumbnail_sizes_as_go() {
        let wide = DynamicImage::new_rgb8(1000, 500);
        assert_eq!(thumbnail(&wide).dimensions_tuple(), (120, 60));
        let tall = DynamicImage::new_rgb8(300, 900);
        assert_eq!(thumbnail(&tall).dimensions_tuple(), (33, 100));
        let small = DynamicImage::new_rgb8(10, 10);
        assert_eq!(thumbnail(&small).dimensions_tuple(), (100, 100));
        assert_eq!(preview(&DynamicImage::new_rgb8(4000, 3000)).dimensions_tuple(), (1920, 1440));
    }

    #[test]
    fn gif_frame_count() {
        let mut out = vec![];
        {
            let mut enc = image::codecs::gif::GifEncoder::new(&mut out);
            for _ in 0..3 {
                enc.encode_frame(image::Frame::new(image::RgbaImage::new(4, 4))).unwrap();
            }
        }
        assert_eq!(gif_frames(&out), Some(3));
    }

    trait Dims {
        fn dimensions_tuple(&self) -> (u32, u32);
    }
    impl Dims for DynamicImage {
        fn dimensions_tuple(&self) -> (u32, u32) {
            (self.width(), self.height())
        }
    }
}

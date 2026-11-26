//! Decode → resize → encode. Everything happens in memory so a failure never
//! leaves a half-written file behind.

use super::{ImageFormat, ImageOps};
use anyhow::{Context, Result};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
use image::codecs::webp::WebPEncoder;
use image::imageops::FilterType;
use image::DynamicImage;
use std::path::Path;

/// Formats Shelf can read.
pub fn can_decode(ext: &str) -> bool {
    matches!(ext.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp")
}

/// Applies `ops` to the image at `path`; returns the encoded bytes and format.
pub fn process(path: &Path, ops: &ImageOps, src_ext: &str) -> Result<(Vec<u8>, ImageFormat)> {
    let mut img = image::open(path).with_context(|| format!("could not read {}", path.display()))?;

    if let Some((max_w, max_h)) = ops.resize {
        img = fit(img, max_w, max_h);
    }

    let format = ops.output_format(src_ext);
    let mut out = Vec::new();
    match format {
        ImageFormat::Jpeg => {
            let quality = ops.compress.unwrap_or(90).clamp(1, 100);
            let rgb = flatten_on_white(&img);
            img_write(&DynamicImage::ImageRgb8(rgb), JpegEncoder::new_with_quality(&mut out, quality))?;
        }
        ImageFormat::Png => {
            let level = if ops.compress.is_some() { CompressionType::Best } else { CompressionType::Default };
            img_write(&img, PngEncoder::new_with_quality(&mut out, level, PngFilter::Adaptive))?;
        }
        ImageFormat::Webp => {
            let img = if img.color().has_alpha() {
                DynamicImage::ImageRgba8(img.to_rgba8())
            } else {
                DynamicImage::ImageRgb8(img.to_rgb8())
            };
            img_write(&img, WebPEncoder::new_lossless(&mut out))?;
        }
    }
    Ok((out, format))
}

fn img_write(img: &DynamicImage, encoder: impl image::ImageEncoder) -> Result<()> {
    img.write_with_encoder(encoder).context("could not encode the image")
}

/// Shrinks to fit inside `max_w` × `max_h` (0 = unlimited), keeping the
/// aspect ratio. Never enlarges.
fn fit(img: DynamicImage, max_w: u32, max_h: u32) -> DynamicImage {
    let (w, h) = (img.width() as f64, img.height() as f64);
    let sx = if max_w == 0 { f64::INFINITY } else { max_w as f64 / w };
    let sy = if max_h == 0 { f64::INFINITY } else { max_h as f64 / h };
    let scale = sx.min(sy).min(1.0);
    if scale >= 1.0 {
        return img;
    }
    let nw = ((w * scale).round() as u32).max(1);
    let nh = ((h * scale).round() as u32).max(1);
    img.resize_exact(nw, nh, FilterType::Lanczos3)
}

/// JPEG has no transparency: composite onto white.
fn flatten_on_white(img: &DynamicImage) -> image::RgbImage {
    if !img.color().has_alpha() {
        return img.to_rgb8();
    }
    let rgba = img.to_rgba8();
    let mut out = image::RgbImage::new(rgba.width(), rgba.height());
    for (x, y, px) in rgba.enumerate_pixels() {
        let a = px[3] as f32 / 255.0;
        let blend = |c: u8| (c as f32 * a + 255.0 * (1.0 - a)).round() as u8;
        out.put_pixel(x, y, image::Rgb([blend(px[0]), blend(px[1]), blend(px[2])]));
    }
    out
}

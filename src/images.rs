use std::io::Cursor;

use anyhow::{bail, Context, Result};
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat, ImageReader, Limits, RgbImage};

use crate::config::{MAX_IMAGE_BYTES, WEBP_QUALITY, WEBP_SKIP_BYTES};

const MAX_PIXELS: u64 = 40_000_000;

fn load_image(data: &[u8]) -> Result<DynamicImage> {
    let mut limits = Limits::default();
    limits.max_image_width = Some(20_000);
    limits.max_image_height = Some(20_000);
    limits.max_alloc = Some(256 * 1024 * 1024);
    let mut reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .context("图片格式无法识别")?;
    reader.limits(limits);
    reader.decode().context("图片格式无法识别")
}

pub fn validate_image(data: &[u8], ext: &str) -> Result<(u32, u32)> {
    let ext = ext.to_ascii_lowercase();
    if !matches!(ext.as_str(), ".webp" | ".png" | ".jpg" | ".jpeg" | ".gif") {
        bail!("不支持的图片格式");
    }
    let image = load_image(data).map_err(|_| anyhow::anyhow!("图片格式无法识别"))?;
    let (width, height) = (image.width(), image.height());
    if width == 0 || height == 0 || width > 20_000 || height > 20_000 {
        bail!("图片尺寸无效");
    }
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        bail!("图片尺寸无效");
    }
    Ok((width, height))
}

fn exif_orientation(data: &[u8]) -> Option<u32> {
    let mut cursor = Cursor::new(data);
    let exif = exif::Reader::new().read_from_container(&mut cursor).ok()?;
    let field = exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)?;
    field.value.get_uint(0)
}

fn apply_orientation(image: DynamicImage, orientation: Option<u32>) -> DynamicImage {
    match orientation.unwrap_or(1) {
        2 => image.fliph(),
        3 => image.rotate180(),
        4 => image.flipv(),
        5 => image.rotate90().fliph(),
        6 => image.rotate90(),
        7 => image.rotate270().fliph(),
        8 => image.rotate270(),
        _ => image,
    }
}

fn flatten_to_rgb(image: DynamicImage) -> RgbImage {
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    let mut rgb = RgbImage::new(width, height);
    for (x, y, pixel) in rgba.enumerate_pixels() {
        let [r, g, b, a] = pixel.0;
        if a == 255 {
            rgb.put_pixel(x, y, image::Rgb([r, g, b]));
        } else {
            let alpha = u16::from(a);
            let blend = |channel: u8| -> u8 {
                let value = (u16::from(channel) * alpha + 255 * (255 - alpha)) / 255;
                value as u8
            };
            rgb.put_pixel(x, y, image::Rgb([blend(r), blend(g), blend(b)]));
        }
    }
    rgb
}

fn encode_jpeg(image: &RgbImage, quality: u8) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut output, quality);
    encoder.encode_image(image).context("图片压缩失败")?;
    Ok(output)
}

pub fn optimize_image(data: Vec<u8>, ext: &str, max_edge: u32) -> Result<(Vec<u8>, String)> {
    let ext = ext.to_ascii_lowercase();
    if !matches!(ext.as_str(), ".webp" | ".png" | ".jpg" | ".jpeg" | ".gif") {
        bail!("不支持的图片格式");
    }
    let mut image = load_image(&data).map_err(|_| anyhow::anyhow!("图片格式无法识别"))?;
    let (width, height) = (image.width(), image.height());
    if width == 0 || height == 0 || width > 20_000 || height > 20_000 {
        bail!("图片尺寸无效");
    }
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        bail!("图片尺寸无效");
    }
    if ext == ".webp" && width.max(height) <= max_edge && data.len() <= WEBP_SKIP_BYTES {
        return Ok((data, ext));
    }
    image = apply_orientation(image, exif_orientation(&data));
    let resized = image.width().max(image.height()) > max_edge;
    if resized {
        image = image.resize(max_edge, max_edge, FilterType::Lanczos3);
    }
    let rgb = flatten_to_rgb(image);
    let mut encoded = encode_jpeg(&rgb, WEBP_QUALITY)?;
    if encoded.len() > MAX_IMAGE_BYTES {
        encoded = encode_jpeg(&rgb, 60)?;
    }
    if encoded.len() > MAX_IMAGE_BYTES {
        encoded = encode_jpeg(&rgb, 45)?;
    }
    if encoded.len() > MAX_IMAGE_BYTES {
        encoded = encode_jpeg(&rgb, 30)?;
    }
    if encoded.len() > MAX_IMAGE_BYTES {
        bail!("图片压缩后仍然过大");
    }
    if !resized && encoded.len() >= data.len() && matches!(ext.as_str(), ".jpg" | ".jpeg" | ".webp")
    {
        return Ok((data, ext));
    }
    Ok((encoded, ".jpg".to_string()))
}

pub fn image_format_for_ext(ext: &str) -> Option<ImageFormat> {
    match ext {
        ".png" => Some(ImageFormat::Png),
        ".jpg" | ".jpeg" => Some(ImageFormat::Jpeg),
        ".webp" => Some(ImageFormat::WebP),
        ".gif" => Some(ImageFormat::Gif),
        _ => None,
    }
}

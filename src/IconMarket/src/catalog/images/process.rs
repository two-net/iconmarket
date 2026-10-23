//! Turning an upload into the JPEG that is stored (FR-PROD-04, NFR-SEC-06): checked by size and
//! by content, never by name; decoded within limits; turned upright; shrunk; and re-encoded,
//! which leaves every piece of metadata behind, GPS location included.

use std::io::Cursor;

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{
    DynamicImage, ExtendedColorType, ImageDecoder, ImageEncoder, ImageFormat, ImageReader,
    ImageResult, Limits, Rgb, RgbImage, Rgba,
};

use super::Rejected;
use crate::config::ImageConfig;

/// `image.maxUploadMB` counts mebibytes
const MB: usize = 1024 * 1024;
/// What decoding may allocate: a 48-megapixel photo with transparency fits. A larger image is
/// refused before anything is allocated, so a small file can't expand into gigabytes.
const MAX_DECODED_BYTES: u64 = 256 * 1024 * 1024;
/// The longest side decoded, whatever the allocation
const MAX_SIDE_PX: u32 = 16_384;
const JPEG_QUALITY: u8 = 85;

/// An upload as it is stored: a JPEG with no metadata, its longest side at most
/// `image.maxDimensionPx`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Processed {
    pub jpeg: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// The most an upload may hold, from `image.maxUploadMB`.
pub(super) fn max_bytes(config: &ImageConfig) -> usize {
    usize::try_from(config.max_upload_mb)
        .unwrap_or(usize::MAX)
        .saturating_mul(MB)
}

/// Checks `bytes` and re-encodes them. Slow for a large photo, so async code calls
/// [`prepare`](super::prepare) instead.
pub fn process(bytes: &[u8], config: &ImageConfig) -> Result<Processed, Rejected> {
    if bytes.len() > max_bytes(config) {
        return Err(Rejected::TooLarge {
            max_mb: config.max_upload_mb,
        });
    }
    // By the magic bytes: the content decides, never the name
    let format = match image::guess_format(bytes) {
        Ok(format @ (ImageFormat::Jpeg | ImageFormat::Png)) => format,
        _ => return Err(Rejected::NotJpegOrPng),
    };
    let image = decode(bytes, format).map_err(|_| Rejected::Unreadable)?;
    let image = shrink(flatten(image), config.max_dimension_px);

    let (width, height) = image.dimensions();
    let mut jpeg = Vec::new();
    // Writes only the image and a JFIF header: no Exif, XMP, ICC profile or comment
    JpegEncoder::new_with_quality(&mut jpeg, JPEG_QUALITY)
        .write_image(image.as_raw(), width, height, ExtendedColorType::Rgb8)
        .map_err(|_| Rejected::Unreadable)?;
    Ok(Processed {
        jpeg,
        width,
        height,
    })
}

/// Decodes `bytes` within [`MAX_DECODED_BYTES`] and [`MAX_SIDE_PX`], turned as its Exif
/// orientation says, so a photo taken with the phone on its side ends up upright once the Exif
/// is gone.
fn decode(bytes: &[u8], format: ImageFormat) -> ImageResult<DynamicImage> {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SIDE_PX);
    limits.max_image_height = Some(MAX_SIDE_PX);
    limits.max_alloc = Some(MAX_DECODED_BYTES);

    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits.clone());
    let mut decoder = reader.into_decoder()?;
    // As `ImageReader::decode` does: the decoded image itself counts against `max_alloc`
    limits.reserve(decoder.total_bytes())?;
    decoder.set_limits(limits)?;
    let orientation = decoder.orientation()?;
    let mut image = DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    Ok(image)
}

/// JPEG has no transparency, so transparent pixels are laid on white, as a chat shows them,
/// rather than turning black. Done before shrinking, so transparent edges leave no dark fringe.
fn flatten(image: DynamicImage) -> RgbImage {
    if !image.color().has_alpha() {
        return image.into_rgb8();
    }
    let image = image.into_rgba8();
    RgbImage::from_fn(image.width(), image.height(), |x, y| {
        let Rgba([r, g, b, alpha]) = *image.get_pixel(x, y);
        let alpha = u32::from(alpha);
        let on_white = |channel: u8| {
            let mixed = (u32::from(channel) * alpha + 255 * (255 - alpha) + 127) / 255;
            u8::try_from(mixed).expect("a weighted mean of two bytes")
        };
        Rgb([on_white(r), on_white(g), on_white(b)])
    })
}

/// Shrinks `image` so its longest side is at most `max` pixels, keeping its proportions. A
/// smaller image is kept as it is, never enlarged.
fn shrink(image: RgbImage, max: u32) -> RgbImage {
    if image.width().max(image.height()) <= max {
        return image;
    }
    // Triangle widens with the scale, so it averages every source pixel: smooth and fast
    DynamicImage::ImageRgb8(image)
        .resize(max, max, FilterType::Triangle)
        .into_rgb8()
}

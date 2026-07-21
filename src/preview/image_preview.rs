use std::io::{BufReader, Cursor};

use anyhow::{Context, Result, bail};
use image::{ImageDecoder, ImageFormat, ImageReader, Limits};

use super::{
    HighlightKind, HighlightSpan, ImagePreviewFormat, PreviewContent, PreviewFile, PreviewKind,
    PreviewRequest, RgbColor, TerminalImageSize,
    common::{BoundedPreview, CommonFormat, ParseBudget},
};

const MAX_IMAGE_DIMENSION: u32 = 16_384;
const MAX_IMAGE_PIXELS: u64 = 24_000_000;
const MAX_IMAGE_ALLOC_BYTES: u64 = 128 * 1024 * 1024;
const MAX_TERMINAL_COLUMNS: u32 = 160;
const MAX_TERMINAL_ROWS: u32 = 80;

pub(super) fn preview(
    bytes: &[u8],
    format: CommonFormat,
    request: &PreviewRequest<'_>,
) -> Result<PreviewContent> {
    let image_format = image_format(format)?;
    let budget = ParseBudget::new(usize::MAX);
    budget.check_stage()?;

    let (width, height, color) = image_metadata(bytes, image_format, format)?;
    validate_dimensions(width, height)?;

    let output = metadata_output(
        format,
        width,
        height,
        color,
        bytes.len() as u64,
        is_animated(bytes, format),
        request,
    );

    let Some(viewport) = request.terminal_image_size() else {
        return Ok(output
            .finish()
            .with_kind(PreviewKind::Image(preview_image_format(format)?)));
    };

    let mut output = output;
    output.push_line("Terminal preview (press o for the system default app):");
    let mut content = output
        .finish()
        .with_kind(PreviewKind::Image(preview_image_format(format)?));
    if content.truncated {
        return Ok(content);
    }

    let decoded = decode_limited(bytes, image_format, format)?;
    budget.check_stage()?;
    append_terminal_image(&mut content, decoded, viewport, request);
    Ok(content)
}

pub(super) fn metadata_preview(
    file: PreviewFile,
    probe: &[u8],
    format: CommonFormat,
    request: &PreviewRequest<'_>,
) -> Result<PreviewContent> {
    let file_len = file.len();
    let image_format = image_format(format)?;
    let mut reader = ImageReader::with_format(BufReader::new(file), image_format);
    reader.limits(decoder_limits());
    let decoder = reader
        .into_decoder()
        .with_context(|| format!("cannot read {} image header", format.label()))?;
    let (width, height) = decoder.dimensions();
    let color = decoder.color_type();
    validate_dimensions(width, height)?;
    Ok(metadata_output(
        format,
        width,
        height,
        color,
        file_len,
        is_animated(probe, format),
        request,
    )
    .finish()
    .with_kind(PreviewKind::Image(preview_image_format(format)?)))
}

fn metadata_output(
    format: CommonFormat,
    width: u32,
    height: u32,
    color: image::ColorType,
    file_len: u64,
    animated: bool,
    request: &PreviewRequest<'_>,
) -> BoundedPreview {
    let mut output = BoundedPreview::new(request.max_bytes, request.max_lines);
    output.push_line(format!("Format: {}", format.label()));
    output.push_line(format!("Dimensions: {width} x {height}"));
    output.push_line(format!("Color: {color:?}"));
    output.push_line(format!("File size: {file_len} bytes"));
    if animated {
        output.push_line("Animation: yes (first frame only)");
    }
    output.push_line("");
    if request.terminal_image_size().is_none() {
        output.push_line("Press o to open with the system default app.");
    }
    output
}

fn image_metadata(
    bytes: &[u8],
    image_format: ImageFormat,
    format: CommonFormat,
) -> Result<(u32, u32, image::ColorType)> {
    let mut reader = ImageReader::with_format(Cursor::new(bytes), image_format);
    reader.limits(decoder_limits());
    let decoder = reader
        .into_decoder()
        .with_context(|| format!("cannot read {} image header", format.label()))?;
    let (width, height) = decoder.dimensions();
    Ok((width, height, decoder.color_type()))
}

fn decode_limited(
    bytes: &[u8],
    image_format: ImageFormat,
    format: CommonFormat,
) -> Result<image::DynamicImage> {
    let mut reader = ImageReader::with_format(Cursor::new(bytes), image_format);
    reader.limits(decoder_limits());
    reader
        .decode()
        .with_context(|| format!("cannot decode {} image", format.label()))
}

fn decoder_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_IMAGE_ALLOC_BYTES);
    limits
}

fn append_terminal_image(
    content: &mut PreviewContent,
    decoded: image::DynamicImage,
    viewport: TerminalImageSize,
    request: &PreviewRequest<'_>,
) {
    let (pixel_width, pixel_height) = terminal_pixel_dimensions(
        decoded.width(),
        decoded.height(),
        u32::from(viewport.columns).min(MAX_TERMINAL_COLUMNS),
        u32::from(viewport.rows).min(MAX_TERMINAL_ROWS),
    );
    if pixel_width == 0 || pixel_height == 0 {
        return;
    }

    let thumbnail = decoded
        .thumbnail_exact(pixel_width, pixel_height)
        .to_rgba8();
    let mut used_bytes = content.lines.iter().map(String::len).sum::<usize>();
    let cell_rows = pixel_height.div_ceil(2);
    for cell_row in 0..cell_rows {
        let line_bytes = usize::try_from(pixel_width)
            .unwrap_or(usize::MAX)
            .saturating_mul('▀'.len_utf8());
        if content.lines.len() >= request.max_lines
            || used_bytes.saturating_add(line_bytes) > request.max_bytes
        {
            content.truncated = true;
            break;
        }

        let mut line = String::with_capacity(line_bytes);
        let mut highlights = Vec::with_capacity(usize::try_from(pixel_width).unwrap_or(0));
        for column in 0..pixel_width {
            let start = line.len();
            line.push('▀');
            let foreground = pixel_color(thumbnail.get_pixel(column, cell_row * 2).0);
            let background = (cell_row * 2 + 1 < pixel_height)
                .then(|| pixel_color(thumbnail.get_pixel(column, cell_row * 2 + 1).0))
                .flatten();
            highlights.push(HighlightSpan {
                range: start..line.len(),
                kind: HighlightKind::ImagePixel {
                    foreground,
                    background,
                },
            });
        }
        used_bytes = used_bytes.saturating_add(line.len());
        content.lines.push(line);
        content.highlights.push(highlights);
    }
}

fn pixel_color(pixel: [u8; 4]) -> Option<RgbColor> {
    (pixel[3] >= 16).then_some(RgbColor {
        red: pixel[0],
        green: pixel[1],
        blue: pixel[2],
    })
}

fn validate_dimensions(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 {
        bail!("image has zero width or height");
    }
    if width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION {
        bail!(
            "image dimensions {width}x{height} exceed the {MAX_IMAGE_DIMENSION} pixel dimension limit"
        );
    }
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| anyhow::anyhow!("image dimensions overflow the pixel budget"))?;
    if pixels > MAX_IMAGE_PIXELS {
        bail!("image has {pixels} pixels; the preview limit is {MAX_IMAGE_PIXELS}");
    }
    Ok(())
}

fn image_format(format: CommonFormat) -> Result<ImageFormat> {
    match format {
        CommonFormat::Png => Ok(ImageFormat::Png),
        CommonFormat::Jpeg => Ok(ImageFormat::Jpeg),
        CommonFormat::Gif => Ok(ImageFormat::Gif),
        CommonFormat::WebP => Ok(ImageFormat::WebP),
        _ => bail!("{} is not an image format", format.label()),
    }
}

fn preview_image_format(format: CommonFormat) -> Result<ImagePreviewFormat> {
    match format {
        CommonFormat::Png => Ok(ImagePreviewFormat::Png),
        CommonFormat::Jpeg => Ok(ImagePreviewFormat::Jpeg),
        CommonFormat::Gif => Ok(ImagePreviewFormat::Gif),
        CommonFormat::WebP => Ok(ImagePreviewFormat::WebP),
        _ => bail!("{} is not an image format", format.label()),
    }
}

fn terminal_pixel_dimensions(
    width: u32,
    height: u32,
    max_columns: u32,
    max_rows: u32,
) -> (u32, u32) {
    if width == 0 || height == 0 || max_columns == 0 || max_rows == 0 {
        return (0, 0);
    }
    let max_pixel_height = max_rows.saturating_mul(2);
    let scale = (f64::from(max_columns) / f64::from(width))
        .min(f64::from(max_pixel_height) / f64::from(height))
        .min(1.0);
    let pixel_width = (f64::from(width) * scale).round().max(1.0) as u32;
    let pixel_height = (f64::from(height) * scale).round().max(1.0) as u32;
    (
        pixel_width.min(max_columns),
        pixel_height.min(max_pixel_height),
    )
}

fn is_animated(bytes: &[u8], format: CommonFormat) -> bool {
    match format {
        CommonFormat::Gif => bytes.windows(11).any(|window| window == b"NETSCAPE2.0"),
        CommonFormat::WebP => bytes
            .windows(4)
            .any(|window| window == b"ANIM" || window == b"ANMF"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Cursor, path::Path};

    use anyhow::Result;
    use image::{DynamicImage, ImageBuffer, ImageFormat, Rgba};
    use tempfile::tempdir;

    use super::*;

    fn png_bytes(image: &DynamicImage) -> Result<Vec<u8>> {
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, ImageFormat::Png)?;
        Ok(bytes.into_inner())
    }

    #[test]
    fn default_image_preview_is_metadata_only() -> Result<()> {
        let image =
            DynamicImage::ImageRgba8(ImageBuffer::from_pixel(4, 2, Rgba([20, 40, 60, 255])));
        let bytes = png_bytes(&image)?;
        let directory = tempdir()?;
        let path = directory.path().join("sample.png");
        fs::write(&path, &bytes)?;
        let content = preview(
            &bytes,
            CommonFormat::Png,
            &PreviewRequest::new(&path, Path::new("sample.png")).with_limits(8_192, 64),
        )?;
        assert_eq!(content.kind, PreviewKind::Image(ImagePreviewFormat::Png));
        assert_eq!(content.lines[0], "Format: PNG");
        assert_eq!(content.lines[1], "Dimensions: 4 x 2");
        assert!(content.lines.iter().any(|line| line.contains("Press o")));
        assert!(
            content
                .lines
                .iter()
                .all(|line| !line.contains(['@', '#', '%']))
        );
        assert!(content.highlights.iter().all(Vec::is_empty));
        Ok(())
    }

    #[test]
    fn confirmed_terminal_preview_uses_truecolor_half_blocks() -> Result<()> {
        let image = DynamicImage::ImageRgba8(ImageBuffer::from_fn(2, 2, |x, y| match (x, y) {
            (0, 0) => Rgba([255, 0, 0, 255]),
            (0, 1) => Rgba([0, 0, 255, 255]),
            (1, 0) => Rgba([0, 255, 0, 255]),
            _ => Rgba([0, 0, 0, 0]),
        }));
        let bytes = png_bytes(&image)?;
        let request = PreviewRequest::new(Path::new("image.png"), Path::new("image.png"))
            .with_terminal_image_size(2, 1);
        let content = preview(&bytes, CommonFormat::Png, &request)?;
        let image_line = content.lines.len() - 1;
        assert_eq!(content.lines[image_line], "▀▀");
        assert_eq!(content.highlights[image_line].len(), 2);
        assert_eq!(
            content.highlights[image_line][0].kind,
            HighlightKind::ImagePixel {
                foreground: Some(RgbColor {
                    red: 255,
                    green: 0,
                    blue: 0,
                }),
                background: Some(RgbColor {
                    red: 0,
                    green: 0,
                    blue: 255,
                }),
            }
        );
        assert_eq!(
            content.highlights[image_line][1].kind,
            HighlightKind::ImagePixel {
                foreground: Some(RgbColor {
                    red: 0,
                    green: 255,
                    blue: 0,
                }),
                background: None,
            }
        );
        Ok(())
    }

    #[test]
    fn terminal_dimensions_preserve_image_aspect_ratio_and_cell_height() {
        assert_eq!(terminal_pixel_dimensions(100, 100, 64, 32), (64, 64));
        assert_eq!(terminal_pixel_dimensions(400, 100, 40, 40), (40, 10));
        assert_eq!(terminal_pixel_dimensions(100, 400, 40, 20), (10, 40));
        assert_eq!(terminal_pixel_dimensions(100, 100, 0, 20), (0, 0));
    }

    #[test]
    fn enabled_image_formats_decode_through_the_same_metadata_path() -> Result<()> {
        let image =
            DynamicImage::ImageRgba8(ImageBuffer::from_pixel(2, 2, Rgba([40, 80, 120, 255])));
        for (image_format, common_format) in [
            (ImageFormat::Png, CommonFormat::Png),
            (ImageFormat::Jpeg, CommonFormat::Jpeg),
            (ImageFormat::Gif, CommonFormat::Gif),
            (ImageFormat::WebP, CommonFormat::WebP),
        ] {
            let mut bytes = Cursor::new(Vec::new());
            image.write_to(&mut bytes, image_format)?;
            let request = PreviewRequest::new(Path::new("image.bin"), Path::new("image.bin"));
            let content = preview(bytes.get_ref(), common_format, &request)?;
            assert_eq!(
                content.kind,
                PreviewKind::Image(preview_image_format(common_format)?)
            );
            assert_eq!(
                content.lines[0],
                format!("Format: {}", common_format.label())
            );
            assert_eq!(content.lines[1], "Dimensions: 2 x 2");
        }
        Ok(())
    }

    #[test]
    fn animation_markers_are_only_advisory_and_first_frame_bounded() {
        assert!(is_animated(b"GIF89a---NETSCAPE2.0---", CommonFormat::Gif));
        assert!(is_animated(b"RIFF----WEBPANMF", CommonFormat::WebP));
        assert!(!is_animated(b"GIF89a", CommonFormat::Gif));
        assert!(!is_animated(b"ANMF", CommonFormat::Png));
    }

    #[test]
    fn dimension_and_pixel_budgets_fail_before_decode() {
        assert!(validate_dimensions(0, 10).is_err());
        assert!(validate_dimensions(MAX_IMAGE_DIMENSION + 1, 1).is_err());
        assert!(validate_dimensions(6_000, 4_001).is_err());
        assert!(validate_dimensions(6_000, 4_000).is_ok());
    }
}

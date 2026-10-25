//! Files in the chat: picking, sending, previewing and saving. Received files
//! stay in memory until the room is left; nothing is written unless the user
//! saves one.

use std::io::Cursor;

use slint::{Rgba8Pixel, SharedPixelBuffer};

/// Previews are decoded to at most this many pixels across (twice the chat
/// width, for high-density screens).
const PREVIEW_PX: u32 = 520;

pub fn size_text(bytes: usize) -> String {
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{:.0} KB", b as f64 / 1024.0),
        b => format!("{:.1} MB", b as f64 / 1024.0 / 1024.0),
    }
}

/// A small RGBA preview when `data` is an image, decoded with limits since
/// it comes from a friend.
pub fn preview(data: &[u8]) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(256 * 1024 * 1024);
    let mut reader = image::ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    reader.limits(limits);
    let decoded = reader.decode().ok()?;
    let small = if decoded.width() > PREVIEW_PX {
        decoded.thumbnail(PREVIEW_PX, u32::MAX)
    } else {
        decoded
    };
    let rgba = small.to_rgba8();
    Some(SharedPixelBuffer::clone_from_slice(
        rgba.as_raw(),
        rgba.width(),
        rgba.height(),
    ))
}

/// Asks for a file to send. `None` when the user cancels.
pub async fn pick() -> Option<std::path::PathBuf> {
    let handle = rfd::AsyncFileDialog::new()
        .set_title("Send a file")
        .pick_file()
        .await?;
    Some(handle.path().to_path_buf())
}

/// Reads a file to send. `Ok(None)` when it's over the size limit, which is
/// found without reading more than the limit.
pub fn read_to_send(path: &std::path::Path) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read;
    let limit = silent_core::proto::MAX_FILE_BYTES;
    let mut data = Vec::new();
    std::fs::File::open(path)?
        .take(limit + 1)
        .read_to_end(&mut data)?;
    Ok((data.len() as u64 <= limit).then_some(data))
}

/// Asks where to save `data`, then writes it. `Ok(false)` when cancelled.
pub async fn save(name: String, data: Vec<u8>) -> std::io::Result<bool> {
    let Some(handle) = rfd::AsyncFileDialog::new()
        .set_title("Save file")
        .set_file_name(&name)
        .save_file()
        .await
    else {
        return Ok(false);
    };
    handle.write(&data).await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_well() {
        assert_eq!(size_text(12), "12 B");
        assert_eq!(size_text(2048), "2 KB");
        assert_eq!(size_text(3 * 1024 * 1024 + 1), "3.0 MB");
    }

    #[test]
    fn previews_only_for_images() {
        assert!(preview(b"not an image").is_none());
        let mut png = Vec::new();
        image::RgbaImage::from_pixel(1000, 10, image::Rgba([225, 29, 46, 255]))
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let p = preview(&png).unwrap();
        assert_eq!((p.width(), p.height()), (PREVIEW_PX, 5));
    }
}

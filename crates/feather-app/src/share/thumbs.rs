//! Pictures and videos for the web page: which files a browser shows by
//! itself, and small JPEG previews from the Windows thumbnail cache (the
//! same ones Explorer shows, so videos and HEIC photos get one too).

use std::path::Path;

/// The type a browser shows `path` with, when it is a picture or video it
/// can play itself. Nothing else is ever sent to show in the page: no SVG,
/// HTML, or anything else that could run code.
pub fn media_type(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "jpg" | "jpeg" | "jfif" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "ogv" => "video/ogg",
        "mkv" => "video/x-matroska",
        _ => return None,
    })
}

/// The largest preview asked for: big enough to fill a phone screen for
/// pictures a browser can't show by itself (HEIC, TIFF).
pub const MAX_SIDE: u32 = 2048;

/// A JPEG preview at most `side` pixels on its longer edge, or `None` when
/// Windows has none for this file.
#[cfg(windows)]
pub fn thumbnail(path: &Path, side: u32) -> Option<Vec<u8>> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::Graphics::Gdi::{DeleteObject, HGDIOBJ, HPALETTE};
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, GUID_ContainerFormatJpeg, GUID_WICPixelFormat24bppBGR,
        IWICBitmapFrameEncode, IWICImagingFactory, WICBitmapDitherTypeNone,
        WICBitmapEncoderNoCache, WICBitmapPaletteTypeCustom, WICBitmapUseAlpha,
    };
    use windows::Win32::System::Com::StructuredStorage::IPropertyBag2;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_MULTITHREADED, STATFLAG_NONAME, STATSTG, STREAM_SEEK_SET,
    };
    use windows::Win32::UI::Shell::{
        IShellItemImageFactory, SHCreateItemFromParsingName, SHCreateMemStream,
        SIIGBF_THUMBNAILONLY,
    };

    struct Com(bool);
    impl Drop for Com {
        fn drop(&mut self) {
            if self.0 {
                unsafe { CoUninitialize() };
            }
        }
    }

    let side = side.clamp(32, MAX_SIDE) as i32;
    // The Shell doesn't take `\\?\` paths, which shared folders resolve to.
    let text = path.to_string_lossy();
    let plain = match text.strip_prefix(r"\\?\") {
        Some(rest) => match rest.strip_prefix(r"UNC\") {
            Some(share) => format!(r"\\{share}"),
            None => rest.to_string(),
        },
        None => text.to_string(),
    };
    let wide: Vec<u16> = std::ffi::OsStr::new(&plain)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let _com = Com(CoInitializeEx(None, COINIT_MULTITHREADED).is_ok());
        let item: IShellItemImageFactory =
            SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).ok()?;
        let bitmap = item
            .GetImage(SIZE { cx: side, cy: side }, SIIGBF_THUMBNAILONLY)
            .ok()?;
        let encode = || -> windows::core::Result<Vec<u8>> {
            let wic: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
            let source =
                wic.CreateBitmapFromHBITMAP(bitmap, HPALETTE::default(), WICBitmapUseAlpha)?;
            let (mut width, mut height) = (0, 0);
            source.GetSize(&mut width, &mut height)?;
            let converter = wic.CreateFormatConverter()?;
            converter.Initialize(
                &source,
                &GUID_WICPixelFormat24bppBGR,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )?;
            let stream = SHCreateMemStream(None).ok_or_else(windows::core::Error::from_thread)?;
            let encoder = wic.CreateEncoder(&GUID_ContainerFormatJpeg, std::ptr::null())?;
            encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;
            let mut frame: Option<IWICBitmapFrameEncode> = None;
            let mut options: Option<IPropertyBag2> = None;
            encoder.CreateNewFrame(&mut frame, &mut options)?;
            let frame = frame.ok_or_else(windows::core::Error::from_thread)?;
            frame.Initialize(options.as_ref())?;
            frame.SetSize(width, height)?;
            let mut format = GUID_WICPixelFormat24bppBGR;
            frame.SetPixelFormat(&mut format)?;
            frame.WriteSource(&converter, std::ptr::null())?;
            frame.Commit()?;
            encoder.Commit()?;
            let mut stat = STATSTG::default();
            stream.Stat(&mut stat, STATFLAG_NONAME)?;
            stream.Seek(0, STREAM_SEEK_SET, None)?;
            let mut out = vec![0u8; stat.cbSize as usize];
            let mut read = 0u32;
            stream
                .Read(out.as_mut_ptr().cast(), out.len() as u32, Some(&mut read))
                .ok()?;
            out.truncate(read as usize);
            Ok(out)
        };
        let jpeg = encode();
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        jpeg.ok().filter(|bytes| !bytes.is_empty())
    }
}

/// Elsewhere the page falls back to the picture itself.
#[cfg(not(windows))]
pub fn thumbnail(_path: &Path, _side: u32) -> Option<Vec<u8>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_media_is_shown_in_the_page() {
        assert_eq!(media_type(Path::new("a/Photo.JPG")), Some("image/jpeg"));
        assert_eq!(media_type(Path::new("clip.mp4")), Some("video/mp4"));
        assert_eq!(media_type(Path::new("page.html")), None);
        assert_eq!(media_type(Path::new("logo.svg")), None);
        assert_eq!(media_type(Path::new("noext")), None);
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    #[test]
    fn windows_makes_jpeg_previews() {
        let wallpapers = std::path::Path::new(r"C:\Windows\Web\Wallpaper\Windows");
        let Some(picture) = std::fs::read_dir(wallpapers)
            .ok()
            .and_then(|mut entries| entries.find_map(|entry| Some(entry.ok()?.path())))
        else {
            return;
        };
        let jpeg = super::thumbnail(&picture, 256).expect("a preview");
        assert_eq!(&jpeg[..2], &[0xff, 0xd8], "a JPEG");
        assert!(jpeg.len() < 200_000);
    }
}

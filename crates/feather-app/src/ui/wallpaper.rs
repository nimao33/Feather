//! The background picture from the custom theme. Choosing one copies it
//! into Feather's own folder, so it keeps working if the original moves;
//! the window decodes it once and draws it under everything.

use std::path::{Path, PathBuf};

use windows::core::{Interface, HSTRING};
use windows::Win32::Foundation::GENERIC_READ;
use windows::Win32::Graphics::Direct2D::{ID2D1Bitmap, ID2D1RenderTarget};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPBGRA, IWICBitmap, IWICBitmapSource,
    IWICImagingFactory, WICBitmapCacheOnLoad, WICBitmapDitherTypeNone,
    WICBitmapInterpolationModeFant, WICBitmapPaletteTypeMedianCut, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};

use super::gfx::Rect;
use crate::settings::BackgroundFit;

/// Bigger pictures are scaled down when loaded; a window is never wider.
const MAX_SIDE: u32 = 3840;
/// Pictures larger than this on disk are refused.
const MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Where chosen pictures are kept.
pub fn folder() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(appdata).join("Feather").join("backgrounds"))
}

/// Checks that `source` is a picture Windows can read and copies it into
/// Feather's folder. Returns the copy's path.
pub fn import(source: &str) -> Result<String, String> {
    let source = Path::new(source);
    let size = std::fs::metadata(source)
        .map_err(|error| format!("Could not read the picture: {error}"))?
        .len();
    if size > MAX_BYTES {
        return Err("That picture is over 64 MB. Choose a smaller one.".into());
    }
    let wic = factory()?;
    decode(&wic, &source.to_string_lossy())
        .map_err(|_| "Windows can't read that file as a picture.".to_string())?;
    let folder = folder().ok_or("Could not find Feather's folder.")?;
    std::fs::create_dir_all(&folder)
        .map_err(|error| format!("Could not create {}: {error}", folder.display()))?;
    let extension = source
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .filter(|extension| {
            !extension.is_empty()
                && extension.len() <= 5
                && extension.chars().all(|c| c.is_ascii_alphanumeric())
        })
        .unwrap_or_else(|| "img".into());
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let target = folder.join(format!("background-{stamp}.{extension}"));
    std::fs::copy(source, &target)
        .map_err(|error| format!("Could not copy the picture: {error}"))?;
    Ok(target.to_string_lossy().to_string())
}

/// Deletes pictures in Feather's folder other than `keep`.
pub fn cleanup(keep: &str) {
    let Some(folder) = folder() else {
        return;
    };
    let Ok(listing) = std::fs::read_dir(&folder) else {
        return;
    };
    for item in listing.flatten() {
        let path = item.path();
        if path.is_file() && !path.to_string_lossy().eq_ignore_ascii_case(keep) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

fn factory() -> Result<IWICImagingFactory, String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
            .map_err(|error| format!("Windows Imaging is unavailable: {error}"))
    }
}

/// Reads the first frame of a picture as premultiplied BGRA, scaled down
/// to `MAX_SIDE` at most.
fn decode(wic: &IWICImagingFactory, path: &str) -> windows::core::Result<IWICBitmap> {
    unsafe {
        let decoder = wic.CreateDecoderFromFilename(
            &HSTRING::from(path),
            None,
            GENERIC_READ,
            WICDecodeMetadataCacheOnDemand,
        )?;
        let frame = decoder.GetFrame(0)?;
        let (mut width, mut height) = (0u32, 0u32);
        frame.GetSize(&mut width, &mut height)?;
        if width == 0 || height == 0 {
            return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                0x8007_000Du32 as i32,
            )));
        }
        let longest = width.max(height);
        let source: IWICBitmapSource = if longest > MAX_SIDE {
            let scale = f64::from(MAX_SIDE) / f64::from(longest);
            let scaler = wic.CreateBitmapScaler()?;
            scaler.Initialize(
                &frame,
                ((f64::from(width) * scale).round() as u32).max(1),
                ((f64::from(height) * scale).round() as u32).max(1),
                WICBitmapInterpolationModeFant,
            )?;
            scaler.cast()?
        } else {
            frame.cast()?
        };
        let converter = wic.CreateFormatConverter()?;
        converter.Initialize(
            &source,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeMedianCut,
        )?;
        wic.CreateBitmapFromSource(&converter, WICBitmapCacheOnLoad)
    }
}

/// The picture the window draws, decoded once and kept as a device bitmap.
#[derive(Default)]
pub struct Wallpaper {
    path: String,
    source: Option<IWICBitmap>,
    size: (f32, f32),
    bitmap: Option<ID2D1Bitmap>,
    /// Decoding failed; not retried until the path changes.
    failed: bool,
}

impl Wallpaper {
    /// Switches to the picture at `path`; empty for none.
    pub fn set(&mut self, path: &str) {
        if path != self.path {
            *self = Self {
                path: path.to_string(),
                ..Self::default()
            };
        }
    }

    pub fn reset_device(&mut self) {
        self.bitmap = None;
    }

    /// The picture as a bitmap for `rt`, with its size. `cache` is false for
    /// one-off targets such as screenshots.
    pub fn bitmap(
        &mut self,
        wic: &IWICImagingFactory,
        rt: &ID2D1RenderTarget,
        cache: bool,
    ) -> Option<(ID2D1Bitmap, f32, f32)> {
        if self.path.is_empty() || self.failed {
            return None;
        }
        if self.source.is_none() {
            match decode(wic, &self.path) {
                Ok(source) => {
                    let (mut width, mut height) = (0u32, 0u32);
                    unsafe {
                        let _ = source.GetSize(&mut width, &mut height);
                    }
                    self.size = (width as f32, height as f32);
                    self.source = Some(source);
                }
                Err(_) => {
                    self.failed = true;
                    return None;
                }
            }
        }
        if cache {
            if let Some(bitmap) = &self.bitmap {
                return Some((bitmap.clone(), self.size.0, self.size.1));
            }
        }
        let source = self.source.as_ref()?;
        let bitmap = unsafe { rt.CreateBitmapFromWicBitmap(source, None).ok()? };
        if cache {
            self.bitmap = Some(bitmap.clone());
        }
        Some((bitmap, self.size.0, self.size.1))
    }
}

/// Where to draw a `width` by `height` picture over `area`: pairs of the
/// destination and, when cropping, the part of the picture to show.
pub fn placements(
    fit: BackgroundFit,
    width: f32,
    height: f32,
    area: Rect,
) -> Vec<(Rect, Option<Rect>)> {
    if width <= 0.0 || height <= 0.0 || area.width() <= 0.0 || area.height() <= 0.0 {
        return Vec::new();
    }
    let (aw, ah) = (area.width(), area.height());
    let centered = |w: f32, h: f32| {
        let l = area.l + (aw - w) / 2.0;
        let t = area.t + (ah - h) / 2.0;
        Rect::new(l, t, l + w, t + h)
    };
    match fit {
        BackgroundFit::Fill => {
            // Covers the window, cropping the middle of the picture.
            let scale = (aw / width).max(ah / height);
            let (sw, sh) = (aw / scale, ah / scale);
            let (sl, st) = ((width - sw) / 2.0, (height - sh) / 2.0);
            vec![(area, Some(Rect::new(sl, st, sl + sw, st + sh)))]
        }
        BackgroundFit::Fit => {
            let scale = (aw / width).min(ah / height);
            vec![(centered(width * scale, height * scale), None)]
        }
        BackgroundFit::Stretch => vec![(area, None)],
        BackgroundFit::Center => vec![(centered(width, height), None)],
        BackgroundFit::Tile => {
            // Tiny pictures are scaled up so there are never too many tiles.
            let scale = (aw * ah / (width * height) / 400.0).sqrt().max(1.0);
            let (tw, th) = (width * scale, height * scale);
            let mut tiles = Vec::new();
            let mut y = area.t;
            while y < area.b {
                let mut x = area.l;
                while x < area.r {
                    tiles.push((Rect::new(x, y, x + tw, y + th), None));
                    x += tw;
                }
                y += th;
            }
            tiles
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_crops_the_middle_and_fit_letterboxes() {
        let area = Rect::new(0.0, 0.0, 1000.0, 500.0);
        let fill = placements(BackgroundFit::Fill, 1000.0, 1000.0, area);
        let (dest, source) = fill[0];
        assert_eq!(dest, area);
        let source = source.unwrap();
        assert_eq!((source.t, source.b), (250.0, 750.0));
        let fit = placements(BackgroundFit::Fit, 1000.0, 1000.0, area);
        assert_eq!(fit[0].0, Rect::new(250.0, 0.0, 750.0, 500.0));
    }

    #[test]
    fn tiles_stay_few() {
        let area = Rect::new(0.0, 0.0, 1920.0, 1080.0);
        let tiles = placements(BackgroundFit::Tile, 4.0, 4.0, area);
        assert!(tiles.len() <= 500, "{}", tiles.len());
        assert!(placements(BackgroundFit::Tile, 0.0, 4.0, area).is_empty());
    }
}

//! Direct2D and DirectWrite resources for the custom-drawn main window.
//!
//! The window draws itself with Direct2D onto a premultiplied-alpha target so
//! the Windows backdrop (Mica or Acrylic) shows through wherever a surface is
//! translucent. Text uses Segoe UI Variable, the variable system font of
//! Windows 11, with its weight and optical-size axes. Windows 10 does not ship
//! it, so the window falls back to Segoe UI there.

use windows::core::{Interface, HSTRING, PCWSTR};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Factory, ID2D1RenderTarget, ID2D1SolidColorBrush, ID2D1StrokeStyle,
    D2D1_CAP_STYLE_ROUND, D2D1_DASH_STYLE_SOLID, D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_ELLIPSE,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT, D2D1_LINE_JOIN_ROUND,
    D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_RENDER_TARGET_USAGE_NONE,
    D2D1_ROUNDED_RECT, D2D1_STROKE_STYLE_PROPERTIES,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteFactory6, IDWriteFontCollection2,
    IDWriteTextFormat, IDWriteTextLayout, DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_AXIS_TAG,
    DWRITE_FONT_AXIS_TAG_OPTICAL_SIZE, DWRITE_FONT_AXIS_TAG_WEIGHT, DWRITE_FONT_AXIS_VALUE,
    DWRITE_FONT_FAMILY_MODEL_TYPOGRAPHIC, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT, DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
    DWRITE_PARAGRAPH_ALIGNMENT_NEAR, DWRITE_TEXT_ALIGNMENT, DWRITE_TEXT_ALIGNMENT_CENTER,
    DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_TEXT_METRICS,
    DWRITE_TEXT_RANGE, DWRITE_TRIMMING, DWRITE_TRIMMING_GRANULARITY_CHARACTER,
    DWRITE_WORD_WRAPPING_NO_WRAP, DWRITE_WORD_WRAPPING_WRAP,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Imaging::{CLSID_WICImagingFactory, IWICImagingFactory};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows_numerics::Vector2;

const FAMILY: &str = "Segoe UI Variable";
const FALLBACK_FAMILY: &str = "Segoe UI";

/// A straight (not premultiplied) RGBA color.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const fn hex(rgb: u32) -> Self {
        Self {
            r: ((rgb >> 16) & 0xff) as f32 / 255.0,
            g: ((rgb >> 8) & 0xff) as f32 / 255.0,
            b: (rgb & 0xff) as f32 / 255.0,
            a: 1.0,
        }
    }

    pub const fn alpha(self, a: f32) -> Self {
        Self {
            r: self.r,
            g: self.g,
            b: self.b,
            a,
        }
    }

    /// Linear blend from `self` (t = 0) to `other` (t = 1).
    pub fn mix(self, other: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let lerp = |a: f32, b: f32| a + (b - a) * t;
        Self {
            r: lerp(self.r, other.r),
            g: lerp(self.g, other.g),
            b: lerp(self.b, other.b),
            a: lerp(self.a, other.a),
        }
    }

    /// Scales the alpha, for fading something in or out.
    /// The color as `0xRRGGBB`, ignoring alpha.
    pub fn to_rgb(self) -> u32 {
        let byte = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u32;
        (byte(self.r) << 16) | (byte(self.g) << 8) | byte(self.b)
    }

    /// Relative luminance, 0 for black to 1 for white.
    pub fn luminance(self) -> f32 {
        let channel = |value: f32| {
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(self.r) + 0.7152 * channel(self.g) + 0.0722 * channel(self.b)
    }

    pub fn fade(self, opacity: f32) -> Self {
        self.alpha(self.a * opacity.clamp(0.0, 1.0))
    }

    fn d2d(self) -> D2D1_COLOR_F {
        D2D1_COLOR_F {
            r: self.r,
            g: self.g,
            b: self.b,
            a: self.a,
        }
    }
}

/// An axis-aligned rectangle in device-independent pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub l: f32,
    pub t: f32,
    pub r: f32,
    pub b: f32,
}

impl Rect {
    pub const fn new(l: f32, t: f32, r: f32, b: f32) -> Self {
        Self { l, t, r, b }
    }

    pub fn width(&self) -> f32 {
        self.r - self.l
    }

    pub fn height(&self) -> f32 {
        self.b - self.t
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.l && x < self.r && y >= self.t && y < self.b
    }

    pub fn inset(&self, dx: f32, dy: f32) -> Self {
        Self::new(self.l + dx, self.t + dy, self.r - dx, self.b - dy)
    }

    pub fn is_empty(&self) -> bool {
        self.r <= self.l || self.b <= self.t
    }

    fn d2d(&self) -> D2D_RECT_F {
        D2D_RECT_F {
            left: self.l,
            top: self.t,
            right: self.r,
            bottom: self.b,
        }
    }
}

/// Which typeface and alignment a run of text uses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Font {
    /// The lowercase wordmark in the sidebar.
    Wordmark,
    /// The large search text.
    Search,
    /// The file name at the top of the details pane (wraps).
    Title,
    Body,
    BodyBold,
    BodyRight,
    Small,
    SmallRight,
    /// Wrapping body text for the details pane.
    BodyWrap,
    /// Centered body text for empty states.
    BodyCenter,
    /// Bold centered label for the primary button.
    Button,
    /// Regular centered label for secondary buttons.
    ButtonLight,
    /// Section headings in dialogs.
    Heading,
    /// Search syntax examples in Help.
    Code,
    /// Wrapping secondary text under a setting's label.
    SmallWrap,
    /// Windows' own icon font, centered: a glyph such as copy or paste.
    Icon,
}

struct Formats {
    wordmark: IDWriteTextFormat,
    search: IDWriteTextFormat,
    title: IDWriteTextFormat,
    body: IDWriteTextFormat,
    body_bold: IDWriteTextFormat,
    body_right: IDWriteTextFormat,
    small: IDWriteTextFormat,
    small_right: IDWriteTextFormat,
    body_wrap: IDWriteTextFormat,
    body_center: IDWriteTextFormat,
    button: IDWriteTextFormat,
    button_light: IDWriteTextFormat,
    heading: IDWriteTextFormat,
    code: IDWriteTextFormat,
    small_wrap: IDWriteTextFormat,
    icon: IDWriteTextFormat,
}

pub struct Gfx {
    pub d2d: ID2D1Factory,
    pub dwrite: IDWriteFactory,
    pub wic: IWICImagingFactory,
    round: ID2D1StrokeStyle,
    formats: Formats,
}

impl Gfx {
    pub fn new() -> Result<Self, String> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)
                .map_err(|error| format!("Direct2D is unavailable: {error}"))?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)
                .map_err(|error| format!("DirectWrite is unavailable: {error}"))?;
            let wic: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                    .map_err(|error| format!("Windows Imaging is unavailable: {error}"))?;
            let round = d2d
                .CreateStrokeStyle(
                    &D2D1_STROKE_STYLE_PROPERTIES {
                        startCap: D2D1_CAP_STYLE_ROUND,
                        endCap: D2D1_CAP_STYLE_ROUND,
                        dashCap: D2D1_CAP_STYLE_ROUND,
                        lineJoin: D2D1_LINE_JOIN_ROUND,
                        miterLimit: 10.0,
                        dashStyle: D2D1_DASH_STYLE_SOLID,
                        dashOffset: 0.0,
                    },
                    None,
                )
                .map_err(|error| error.to_string())?;
            let collection = variable_font_collection(&dwrite).ok().flatten();
            let formats = Formats::create(&dwrite, collection.as_ref())?;
            Ok(Self {
                d2d,
                dwrite,
                wic,
                round,
                formats,
            })
        }
    }

    fn format(&self, font: Font) -> &IDWriteTextFormat {
        let f = &self.formats;
        match font {
            Font::Wordmark => &f.wordmark,
            Font::Search => &f.search,
            Font::Title => &f.title,
            Font::Body => &f.body,
            Font::BodyBold => &f.body_bold,
            Font::BodyRight => &f.body_right,
            Font::Small => &f.small,
            Font::SmallRight => &f.small_right,
            Font::BodyWrap => &f.body_wrap,
            Font::BodyCenter => &f.body_center,
            Font::Button => &f.button,
            Font::ButtonLight => &f.button_light,
            Font::Heading => &f.heading,
            Font::Code => &f.code,
            Font::SmallWrap => &f.small_wrap,
            Font::Icon => &f.icon,
        }
    }

    /// Lays out `text` for hit testing and caret placement.
    pub fn layout(
        &self,
        text: &[u16],
        font: Font,
        width: f32,
        height: f32,
    ) -> Option<IDWriteTextLayout> {
        unsafe {
            self.dwrite
                .CreateTextLayout(text, self.format(font), width.max(1.0), height.max(1.0))
                .ok()
        }
    }

    /// The height a wrapping text needs at the given width.
    pub fn measure_height(&self, text: &str, font: Font, width: f32) -> f32 {
        let wide: Vec<u16> = text.encode_utf16().collect();
        let Some(layout) = self.layout(&wide, font, width, 10_000.0) else {
            return 0.0;
        };
        let mut metrics = DWRITE_TEXT_METRICS::default();
        unsafe {
            if layout.GetMetrics(&mut metrics).is_err() {
                return 0.0;
            }
        }
        metrics.height
    }

    /// The width of a single line of text.
    pub fn measure_width(&self, text: &str, font: Font) -> f32 {
        let wide: Vec<u16> = text.encode_utf16().collect();
        let Some(layout) = self.layout(&wide, font, 10_000.0, 100.0) else {
            return 0.0;
        };
        let mut metrics = DWRITE_TEXT_METRICS::default();
        unsafe {
            if layout.GetMetrics(&mut metrics).is_err() {
                return 0.0;
            }
        }
        metrics.widthIncludingTrailingWhitespace
    }
}

/// Software rendering keeps the window off the GPU, which keeps memory use
/// low; a frame of this UI is small enough to rasterize on the CPU.
pub fn render_target_properties() -> D2D1_RENDER_TARGET_PROPERTIES {
    D2D1_RENDER_TARGET_PROPERTIES {
        r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: 96.0,
        dpiY: 96.0,
        usage: D2D1_RENDER_TARGET_USAGE_NONE,
        minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
    }
}

/// The system collection with typographic families, so Segoe UI Variable is
/// one family with weight and optical-size axes. `None` when it is missing.
unsafe fn variable_font_collection(
    dwrite: &IDWriteFactory,
) -> windows::core::Result<Option<IDWriteFontCollection2>> {
    let factory6: IDWriteFactory6 = dwrite.cast()?;
    let collection =
        factory6.GetSystemFontCollection(false, DWRITE_FONT_FAMILY_MODEL_TYPOGRAPHIC)?;
    let mut index = 0u32;
    let mut exists = windows::core::BOOL(0);
    collection.FindFamilyName(&HSTRING::from(FAMILY), &mut index, &mut exists)?;
    Ok(exists.as_bool().then_some(collection))
}

/// Windows 11's icon font, or Windows 10's, which has the same glyphs.
unsafe fn icon_family(dwrite: &IDWriteFactory) -> &'static str {
    const FLUENT: &str = "Segoe Fluent Icons";
    let mut collection = None;
    let found = dwrite
        .GetSystemFontCollection(&mut collection, false)
        .ok()
        .and(collection)
        .is_some_and(|collection| {
            let mut index = 0u32;
            let mut exists = windows::core::BOOL(0);
            collection
                .FindFamilyName(&HSTRING::from(FLUENT), &mut index, &mut exists)
                .is_ok()
                && exists.as_bool()
        });
    if found {
        FLUENT
    } else {
        "Segoe MDL2 Assets"
    }
}

struct FormatSpec {
    /// A specific system family instead of Segoe UI Variable.
    family: Option<&'static str>,
    weight: f32,
    size: f32,
    align: DWRITE_TEXT_ALIGNMENT,
    wrap: bool,
    center_vertically: bool,
}

impl Formats {
    unsafe fn create(
        dwrite: &IDWriteFactory,
        collection: Option<&IDWriteFontCollection2>,
    ) -> Result<Self, String> {
        let make = |spec: FormatSpec| -> Result<IDWriteTextFormat, String> {
            make_format(dwrite, collection, spec).map_err(|error| error.to_string())
        };
        let single = |weight, size, align| FormatSpec {
            family: None,
            weight,
            size,
            align,
            wrap: false,
            center_vertically: true,
        };
        let block = |weight, size, align, center_vertically| FormatSpec {
            family: None,
            weight,
            size,
            align,
            wrap: true,
            center_vertically,
        };
        Ok(Self {
            wordmark: make(single(700.0, 22.0, DWRITE_TEXT_ALIGNMENT_LEADING))?,
            search: make(single(400.0, 28.0, DWRITE_TEXT_ALIGNMENT_LEADING))?,
            title: make(block(600.0, 22.0, DWRITE_TEXT_ALIGNMENT_LEADING, false))?,
            body: make(single(400.0, 13.0, DWRITE_TEXT_ALIGNMENT_LEADING))?,
            body_bold: make(single(600.0, 14.0, DWRITE_TEXT_ALIGNMENT_LEADING))?,
            body_right: make(single(400.0, 13.0, DWRITE_TEXT_ALIGNMENT_TRAILING))?,
            small: make(single(400.0, 12.0, DWRITE_TEXT_ALIGNMENT_LEADING))?,
            small_right: make(single(400.0, 12.0, DWRITE_TEXT_ALIGNMENT_TRAILING))?,
            body_wrap: make(block(400.0, 13.0, DWRITE_TEXT_ALIGNMENT_LEADING, false))?,
            body_center: make(block(400.0, 14.0, DWRITE_TEXT_ALIGNMENT_CENTER, true))?,
            button: make(single(600.0, 14.0, DWRITE_TEXT_ALIGNMENT_CENTER))?,
            button_light: make(single(400.0, 13.0, DWRITE_TEXT_ALIGNMENT_CENTER))?,
            heading: make(single(600.0, 15.0, DWRITE_TEXT_ALIGNMENT_LEADING))?,
            code: make(FormatSpec {
                family: Some("Consolas"),
                ..single(400.0, 13.0, DWRITE_TEXT_ALIGNMENT_LEADING)
            })?,
            small_wrap: make(block(400.0, 12.0, DWRITE_TEXT_ALIGNMENT_LEADING, false))?,
            icon: make(FormatSpec {
                family: Some(icon_family(dwrite)),
                ..single(400.0, 15.0, DWRITE_TEXT_ALIGNMENT_CENTER)
            })?,
        })
    }
}

unsafe fn make_format(
    dwrite: &IDWriteFactory,
    collection: Option<&IDWriteFontCollection2>,
    spec: FormatSpec,
) -> windows::core::Result<IDWriteTextFormat> {
    let locale = HSTRING::from("en-us");
    let format: IDWriteTextFormat = match (spec.family, collection) {
        (None, Some(collection)) => {
            let factory6: IDWriteFactory6 = dwrite.cast()?;
            // The optical-size axis is in points; sizes here are in DIPs.
            let axes = [
                axis(DWRITE_FONT_AXIS_TAG_WEIGHT, spec.weight),
                axis(
                    DWRITE_FONT_AXIS_TAG_OPTICAL_SIZE,
                    (spec.size * 0.75).clamp(8.0, 36.0),
                ),
            ];
            factory6
                .CreateTextFormat(
                    &HSTRING::from(FAMILY),
                    collection,
                    &axes,
                    spec.size,
                    &locale,
                )?
                .cast()?
        }
        (family, _) => dwrite.CreateTextFormat(
            &HSTRING::from(family.unwrap_or(FALLBACK_FAMILY)),
            None,
            DWRITE_FONT_WEIGHT(spec.weight as i32),
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            spec.size,
            PCWSTR(locale.as_ptr()),
        )?,
    };
    format.SetTextAlignment(spec.align)?;
    format.SetParagraphAlignment(if spec.center_vertically {
        DWRITE_PARAGRAPH_ALIGNMENT_CENTER
    } else {
        DWRITE_PARAGRAPH_ALIGNMENT_NEAR
    })?;
    if spec.wrap {
        format.SetWordWrapping(DWRITE_WORD_WRAPPING_WRAP)?;
    } else {
        format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
        let sign = dwrite.CreateEllipsisTrimmingSign(&format)?;
        format.SetTrimming(
            &DWRITE_TRIMMING {
                granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                delimiter: 0,
                delimiterCount: 0,
            },
            &sign,
        )?;
    }
    Ok(format)
}

fn axis(tag: DWRITE_FONT_AXIS_TAG, value: f32) -> DWRITE_FONT_AXIS_VALUE {
    DWRITE_FONT_AXIS_VALUE {
        axisTag: tag,
        value,
    }
}

/// One frame of drawing. Wraps a render target and a reusable brush.
pub struct Painter<'a> {
    pub rt: &'a ID2D1RenderTarget,
    pub gfx: &'a Gfx,
    brush: ID2D1SolidColorBrush,
    /// A second brush so one text layout can mix normal and dimmed words.
    dim: ID2D1SolidColorBrush,
}

impl<'a> Painter<'a> {
    pub fn new(rt: &'a ID2D1RenderTarget, gfx: &'a Gfx) -> Option<Self> {
        let brush = unsafe { rt.CreateSolidColorBrush(&Color::hex(0).d2d(), None).ok()? };
        let dim = unsafe { rt.CreateSolidColorBrush(&Color::hex(0).d2d(), None).ok()? };
        Some(Self {
            rt,
            gfx,
            brush,
            dim,
        })
    }

    fn brush(&self, color: Color) -> &ID2D1SolidColorBrush {
        unsafe {
            self.brush.SetColor(&color.d2d());
        }
        &self.brush
    }

    pub fn clear(&self, color: Color) {
        unsafe {
            self.rt.Clear(Some(&color.d2d()));
        }
    }

    pub fn fill(&self, rect: Rect, color: Color) {
        if color.a <= 0.0 || rect.is_empty() {
            return;
        }
        unsafe {
            self.rt.FillRectangle(&rect.d2d(), self.brush(color));
        }
    }

    /// Fills a rounded rectangle with a gradient through `stops` (position
    /// from 0 to 1, color), left to right or top to bottom.
    pub fn fill_gradient(&self, rect: Rect, radius: f32, stops: &[(f32, Color)], vertical: bool) {
        use windows::Win32::Graphics::Direct2D::Common::D2D1_GRADIENT_STOP;
        use windows::Win32::Graphics::Direct2D::{
            D2D1_EXTEND_MODE_CLAMP, D2D1_GAMMA_2_2, D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES,
        };
        if rect.is_empty() || stops.is_empty() {
            return;
        }
        let stops: Vec<D2D1_GRADIENT_STOP> = stops
            .iter()
            .map(|(position, color)| D2D1_GRADIENT_STOP {
                position: *position,
                color: color.d2d(),
            })
            .collect();
        let end = if vertical {
            Vector2 {
                X: rect.l,
                Y: rect.b,
            }
        } else {
            Vector2 {
                X: rect.r,
                Y: rect.t,
            }
        };
        unsafe {
            let Ok(collection) = self.rt.CreateGradientStopCollection(
                &stops,
                D2D1_GAMMA_2_2,
                D2D1_EXTEND_MODE_CLAMP,
            ) else {
                return;
            };
            let Ok(brush) = self.rt.CreateLinearGradientBrush(
                &D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                    startPoint: Vector2 {
                        X: rect.l,
                        Y: rect.t,
                    },
                    endPoint: end,
                },
                None,
                &collection,
            ) else {
                return;
            };
            self.rt.FillRoundedRectangle(
                &D2D1_ROUNDED_RECT {
                    rect: rect.d2d(),
                    radiusX: radius,
                    radiusY: radius,
                },
                &brush,
            );
        }
    }

    pub fn fill_round(&self, rect: Rect, radius: f32, color: Color) {
        if color.a <= 0.0 || rect.is_empty() {
            return;
        }
        unsafe {
            self.rt.FillRoundedRectangle(
                &D2D1_ROUNDED_RECT {
                    rect: rect.d2d(),
                    radiusX: radius,
                    radiusY: radius,
                },
                self.brush(color),
            );
        }
    }

    pub fn stroke_round(&self, rect: Rect, radius: f32, color: Color, width: f32) {
        unsafe {
            self.rt.DrawRoundedRectangle(
                &D2D1_ROUNDED_RECT {
                    rect: rect.inset(width / 2.0, width / 2.0).d2d(),
                    radiusX: radius,
                    radiusY: radius,
                },
                self.brush(color),
                width,
                None,
            );
        }
    }

    pub fn line(&self, x0: f32, y0: f32, x1: f32, y1: f32, color: Color, width: f32) {
        unsafe {
            self.rt.DrawLine(
                Vector2 { X: x0, Y: y0 },
                Vector2 { X: x1, Y: y1 },
                self.brush(color),
                width,
                &self.gfx.round,
            );
        }
    }

    pub fn circle(&self, cx: f32, cy: f32, radius: f32, color: Color, width: f32) {
        unsafe {
            self.rt.DrawEllipse(
                &D2D1_ELLIPSE {
                    point: Vector2 { X: cx, Y: cy },
                    radiusX: radius,
                    radiusY: radius,
                },
                self.brush(color),
                width,
                &self.gfx.round,
            );
        }
    }

    fn path(
        &self,
        points: &[(f32, f32)],
        closed: bool,
        filled: bool,
    ) -> Option<windows::Win32::Graphics::Direct2D::ID2D1PathGeometry> {
        use windows::Win32::Graphics::Direct2D::Common::{
            D2D1_FIGURE_BEGIN_FILLED, D2D1_FIGURE_BEGIN_HOLLOW, D2D1_FIGURE_END_CLOSED,
            D2D1_FIGURE_END_OPEN,
        };
        let (first, rest) = points.split_first()?;
        let point = |&(x, y): &(f32, f32)| Vector2 { X: x, Y: y };
        unsafe {
            let path = self.gfx.d2d.CreatePathGeometry().ok()?;
            let sink = path.Open().ok()?;
            sink.BeginFigure(
                point(first),
                if filled {
                    D2D1_FIGURE_BEGIN_FILLED
                } else {
                    D2D1_FIGURE_BEGIN_HOLLOW
                },
            );
            let rest: Vec<Vector2> = rest.iter().map(point).collect();
            sink.AddLines(&rest);
            sink.EndFigure(if closed {
                D2D1_FIGURE_END_CLOSED
            } else {
                D2D1_FIGURE_END_OPEN
            });
            sink.Close().ok()?;
            Some(path)
        }
    }

    /// Fills the polygon through `points`.
    pub fn polygon(&self, points: &[(f32, f32)], color: Color) {
        if color.a <= 0.0 {
            return;
        }
        if let Some(path) = self.path(points, true, true) {
            unsafe {
                self.rt.FillGeometry(&path, self.brush(color), None);
            }
        }
    }

    /// Strokes the line through `points`, closed back to the start if asked.
    pub fn polyline(&self, points: &[(f32, f32)], closed: bool, color: Color, width: f32) {
        if let Some(path) = self.path(points, closed, false) {
            unsafe {
                self.rt
                    .DrawGeometry(&path, self.brush(color), width, &self.gfx.round);
            }
        }
    }

    /// A circular arrow: "do this again".
    pub fn refresh(&self, cx: f32, cy: f32, radius: f32, color: Color, width: f32) {
        let point = |degrees: f32, r: f32| {
            let (s, c) = degrees.to_radians().sin_cos();
            (cx + c * r, cy + s * r)
        };
        // Most of a circle, clockwise from the top right, with a gap at the
        // top for the arrow to point into.
        let (from, to) = (-55.0_f32, 228.0_f32);
        let steps = 36;
        let arc: Vec<(f32, f32)> = (0..=steps)
            .map(|step| point(from + (to - from) * step as f32 / steps as f32, radius))
            .collect();
        self.polyline(&arc, false, color, width);
        // A solid arrowhead at the end, pointing on round the circle.
        let end = to + 4.0;
        let (s, c) = end.to_radians().sin_cos();
        let (tx, ty) = (-s, c);
        let (bx, by) = point(end, radius);
        let head = radius * 0.95;
        self.polygon(
            &[
                (bx + c * head * 0.62, by + s * head * 0.62),
                (bx + tx * head, by + ty * head),
                (bx - c * head * 0.62, by - s * head * 0.62),
            ],
            color,
        );
    }

    /// A crescent moon outline, drawn as two arcs between the same points.
    pub fn moon(&self, cx: f32, cy: f32, radius: f32, color: Color, width: f32) {
        use windows::Win32::Graphics::Direct2D::Common::{
            D2D1_FIGURE_BEGIN_HOLLOW, D2D1_FIGURE_END_CLOSED, D2D_SIZE_F,
        };
        use windows::Win32::Graphics::Direct2D::{
            D2D1_ARC_SEGMENT, D2D1_ARC_SIZE_LARGE, D2D1_ARC_SIZE_SMALL,
            D2D1_SWEEP_DIRECTION_CLOCKWISE, D2D1_SWEEP_DIRECTION_COUNTER_CLOCKWISE,
        };
        let point = |degrees: f32| {
            let (s, c) = degrees.to_radians().sin_cos();
            Vector2 {
                X: cx + c * radius,
                Y: cy + s * radius,
            }
        };
        let (top, side) = (point(-70.0), point(20.0));
        unsafe {
            let Ok(path) = self.gfx.d2d.CreatePathGeometry() else {
                return;
            };
            let Ok(sink) = path.Open() else {
                return;
            };
            sink.BeginFigure(top, D2D1_FIGURE_BEGIN_HOLLOW);
            sink.AddArc(&D2D1_ARC_SEGMENT {
                point: side,
                size: D2D_SIZE_F {
                    width: radius,
                    height: radius,
                },
                rotationAngle: 0.0,
                sweepDirection: D2D1_SWEEP_DIRECTION_COUNTER_CLOCKWISE,
                arcSize: D2D1_ARC_SIZE_LARGE,
            });
            sink.AddArc(&D2D1_ARC_SEGMENT {
                point: top,
                size: D2D_SIZE_F {
                    width: radius * 0.78,
                    height: radius * 0.78,
                },
                rotationAngle: 0.0,
                sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
                arcSize: D2D1_ARC_SIZE_SMALL,
            });
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);
            if sink.Close().is_err() {
                return;
            }
            self.rt
                .DrawGeometry(&path, self.brush(color), width, &self.gfx.round);
        }
    }

    pub fn text(&self, text: &str, font: Font, rect: Rect, color: Color) {
        if text.is_empty() || rect.is_empty() {
            return;
        }
        let wide: Vec<u16> = text.encode_utf16().collect();
        self.text_wide(&wide, font, rect, color);
    }

    pub fn text_wide(&self, text: &[u16], font: Font, rect: Rect, color: Color) {
        unsafe {
            self.rt.DrawText(
                text,
                self.gfx.format(font),
                &rect.d2d(),
                self.brush(color),
                D2D1_DRAW_TEXT_OPTIONS_NONE,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }

    pub fn text_layout(&self, layout: &IDWriteTextLayout, x: f32, y: f32, color: Color) {
        unsafe {
            self.rt.DrawTextLayout(
                Vector2 { X: x, Y: y },
                layout,
                self.brush(color),
                D2D1_DRAW_TEXT_OPTIONS_NONE,
            );
        }
    }

    /// Draws one line with the parts outside `spans` in `muted`, so the words
    /// that matched stand out and the rest of the name recedes. Spans are in
    /// UTF-16 units of `text`.
    pub fn text_marked(
        &self,
        text: &str,
        font: Font,
        rect: Rect,
        color: Color,
        muted: Color,
        spans: &[(usize, usize)],
    ) {
        if text.is_empty() || rect.is_empty() {
            return;
        }
        let wide: Vec<u16> = text.encode_utf16().collect();
        let Some(layout) = self.gfx.layout(&wide, font, rect.width(), rect.height()) else {
            self.text_wide(&wide, font, rect, color);
            return;
        };
        unsafe {
            self.dim.SetColor(&muted.d2d());
        }
        // Mark everything the search did not match with the dim brush; the
        // layout draws unmatched runs with it and matched runs with `color`.
        let mut at = 0usize;
        for &(start, end) in spans {
            let start = start.min(wide.len());
            let end = end.min(wide.len());
            if start > at {
                unsafe {
                    let _ = layout.SetDrawingEffect(
                        &self.dim,
                        DWRITE_TEXT_RANGE {
                            startPosition: at as u32,
                            length: (start - at) as u32,
                        },
                    );
                }
            }
            at = at.max(end);
        }
        if at < wide.len() {
            unsafe {
                let _ = layout.SetDrawingEffect(
                    &self.dim,
                    DWRITE_TEXT_RANGE {
                        startPosition: at as u32,
                        length: (wide.len() - at) as u32,
                    },
                );
            }
        }
        unsafe {
            self.rt.DrawTextLayout(
                Vector2 {
                    X: rect.l,
                    Y: rect.t,
                },
                &layout,
                self.brush(color),
                D2D1_DRAW_TEXT_OPTIONS_NONE,
            );
        }
    }

    pub fn clip(&self, rect: Rect) {
        unsafe {
            self.rt.PushAxisAlignedClip(
                &rect.d2d(),
                windows::Win32::Graphics::Direct2D::D2D1_ANTIALIAS_MODE_ALIASED,
            );
        }
    }

    pub fn unclip(&self) {
        unsafe {
            self.rt.PopAxisAlignedClip();
        }
    }

    pub fn bitmap(
        &self,
        bitmap: &windows::Win32::Graphics::Direct2D::ID2D1Bitmap,
        dest: Rect,
        source: Option<Rect>,
        opacity: f32,
    ) {
        let source = source.map(|rect| rect.d2d());
        unsafe {
            self.rt.DrawBitmap(
                bitmap,
                Some(&dest.d2d()),
                opacity.clamp(0.0, 1.0),
                windows::Win32::Graphics::Direct2D::D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                source.as_ref().map(|rect| rect as *const _),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_colors_decode() {
        let color = Color::hex(0xa85a06);
        assert!((color.r - 0xa8 as f32 / 255.0).abs() < 1e-6);
        assert!((color.g - 0x5a as f32 / 255.0).abs() < 1e-6);
        assert!((color.b - 0x06 as f32 / 255.0).abs() < 1e-6);
        assert_eq!(color.alpha(0.5).a, 0.5);
    }

    #[test]
    fn colors_mix_and_fade() {
        let black = Color::hex(0x000000);
        let white = Color::hex(0xffffff);
        let mid = black.mix(white, 0.5);
        assert!((mid.r - 0.5).abs() < 1e-6);
        assert_eq!(black.mix(white, 2.0), white);
        assert_eq!(white.alpha(0.8).fade(0.5).a, 0.4);
    }

    #[test]
    fn rect_hit_testing_is_half_open() {
        let rect = Rect::new(10.0, 10.0, 20.0, 20.0);
        assert!(rect.contains(10.0, 10.0));
        assert!(!rect.contains(20.0, 15.0));
        assert!(rect.inset(6.0, 0.0).is_empty());
    }
}

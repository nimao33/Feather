//! Color models for the color picker: OKLCH (even steps the eye agrees
//! with), RGB, and HSL. Colors are `0xRRGGBB`; each model shows three values
//! in the units people know (0 to 100, 0 to 255, degrees).

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Model {
    Oklch,
    Rgb,
    Hsl,
}

impl Model {
    pub const ALL: [Model; 3] = [Model::Oklch, Model::Rgb, Model::Hsl];

    pub fn name(self) -> &'static str {
        match self {
            Model::Oklch => "OKLCH",
            Model::Rgb => "RGB",
            Model::Hsl => "HSL",
        }
    }

    pub fn next(self) -> Model {
        match self {
            Model::Oklch => Model::Rgb,
            Model::Rgb => Model::Hsl,
            Model::Hsl => Model::Oklch,
        }
    }

    /// Each value's largest amount.
    pub fn ranges(self) -> [f32; 3] {
        match self {
            Model::Oklch => [100.0, 100.0, 360.0],
            Model::Rgb => [255.0, 255.0, 255.0],
            Model::Hsl => [360.0, 100.0, 100.0],
        }
    }

    pub fn labels(self) -> [&'static str; 3] {
        match self {
            Model::Oklch => ["Lightness", "Chroma", "Hue"],
            Model::Rgb => ["Red", "Green", "Blue"],
            Model::Hsl => ["Hue", "Saturation", "Lightness"],
        }
    }

    /// The color for these values; a color past what a screen shows keeps
    /// its lightness and hue and loses chroma until it fits.
    pub fn to_rgb(self, values: [f32; 3]) -> u32 {
        match self {
            Model::Oklch => {
                oklch_to_rgb(values[0] / 100.0, values[1] / 100.0 * MAX_CHROMA, values[2])
            }
            Model::Rgb => pack(values[0] / 255.0, values[1] / 255.0, values[2] / 255.0),
            Model::Hsl => hsl_to_rgb(values[0], values[1] / 100.0, values[2] / 100.0),
        }
    }

    /// The values for a color. `keep` is what was shown before: grays have
    /// no hue, so the earlier hue stays rather than jumping to red.
    pub fn from_rgb(self, rgb: u32, keep: [f32; 3]) -> [f32; 3] {
        let (r, g, b) = unpack(rgb);
        match self {
            Model::Oklch => {
                let (l, a, bb) = to_oklab(r, g, b);
                let chroma = (a * a + bb * bb).sqrt();
                let hue = if chroma < 0.002 {
                    keep[2]
                } else {
                    bb.atan2(a).to_degrees().rem_euclid(360.0)
                };
                [l * 100.0, (chroma / MAX_CHROMA * 100.0).min(100.0), hue]
            }
            Model::Rgb => [r * 255.0, g * 255.0, b * 255.0],
            Model::Hsl => {
                let max = r.max(g).max(b);
                let min = r.min(g).min(b);
                let light = (max + min) / 2.0;
                let delta = max - min;
                if delta < 1e-6 {
                    return [keep[0], 0.0, light * 100.0];
                }
                let saturation = delta / (1.0 - (2.0 * light - 1.0).abs()).max(1e-6);
                let hue = if max == r {
                    60.0 * ((g - b) / delta).rem_euclid(6.0)
                } else if max == g {
                    60.0 * ((b - r) / delta + 2.0)
                } else {
                    60.0 * ((r - g) / delta + 4.0)
                };
                [hue, (saturation * 100.0).min(100.0), light * 100.0]
            }
        }
    }

    /// Colors along one value's track, the others held, to paint the track.
    pub fn track(self, values: [f32; 3], channel: usize) -> Vec<(f32, u32)> {
        let steps = match (self, channel) {
            (Model::Rgb, _) => 2,
            (Model::Hsl, 0) | (Model::Oklch, 2) => 12,
            _ => 8,
        };
        let range = self.ranges()[channel];
        (0..=steps)
            .map(|step| {
                let t = step as f32 / steps as f32;
                let mut at = values;
                at[channel] = t * range;
                (t, self.to_rgb(at))
            })
            .collect()
    }
}

/// The chroma at 100 on the slider; sRGB colors stay under it.
const MAX_CHROMA: f32 = 0.37;

fn unpack(rgb: u32) -> (f32, f32, f32) {
    let channel = |shift: u32| ((rgb >> shift) & 0xff) as f32 / 255.0;
    (channel(16), channel(8), channel(0))
}

fn pack(r: f32, g: f32, b: f32) -> u32 {
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
    (byte(r) << 16) | (byte(g) << 8) | byte(b)
}

fn to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn from_linear(c: f32) -> f32 {
    if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

fn to_oklab(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let (r, g, b) = (to_linear(r), to_linear(g), to_linear(b));
    let l = (0.412_221_47 * r + 0.536_332_55 * g + 0.051_445_995 * b).cbrt();
    let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b).cbrt();
    (
        0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    )
}

fn from_oklab(l: f32, a: f32, b: f32) -> (f32, f32, f32) {
    let l_ = l + 0.396_337_78 * a + 0.215_803_76 * b;
    let m_ = l - 0.105_561_346 * a - 0.063_854_17 * b;
    let s_ = l - 0.089_484_18 * a - 1.291_485_5 * b;
    let (l3, m3, s3) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
    (
        from_linear(4.076_741_7 * l3 - 3.307_711_6 * m3 + 0.230_969_94 * s3),
        from_linear(-1.268_438 * l3 + 2.609_757_4 * m3 - 0.341_319_38 * s3),
        from_linear(-0.004_196_086_3 * l3 - 0.703_418_6 * m3 + 1.707_614_7 * s3),
    )
}

fn oklch_to_rgb(l: f32, chroma: f32, hue: f32) -> u32 {
    let (sin, cos) = hue.to_radians().sin_cos();
    let at = |c: f32| from_oklab(l, c * cos, c * sin);
    let fits = |(r, g, b): (f32, f32, f32)| {
        let ok = |v: f32| (-0.001..=1.001).contains(&v);
        ok(r) && ok(g) && ok(b)
    };
    if fits(at(chroma)) {
        let (r, g, b) = at(chroma);
        return pack(r, g, b);
    }
    // Past the screen's colors: the most chroma that still fits.
    let (mut low, mut high) = (0.0, chroma);
    for _ in 0..14 {
        let mid = (low + high) / 2.0;
        if fits(at(mid)) {
            low = mid;
        } else {
            high = mid;
        }
    }
    let (r, g, b) = at(low);
    pack(r, g, b)
}

fn hsl_to_rgb(hue: f32, saturation: f32, light: f32) -> u32 {
    let c = (1.0 - (2.0 * light - 1.0).abs()) * saturation;
    let h = hue.rem_euclid(360.0) / 60.0;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = light - c / 2.0;
    pack(r + m, g + m, b + m)
}

/// `#RRGGBB`, `RRGGBB`, `#RGB`, or `rgb(r, g, b)`, as typed or pasted.
pub fn parse(text: &str) -> Option<u32> {
    let text = text.trim();
    if let Some(inner) = text
        .strip_prefix("rgb(")
        .or_else(|| text.strip_prefix("rgba("))
        .and_then(|rest| rest.strip_suffix(')'))
    {
        let parts: Vec<u32> = inner
            .split(',')
            .take(3)
            .map(|part| part.trim().parse::<u32>().ok().filter(|v| *v <= 255))
            .collect::<Option<_>>()?;
        return (parts.len() == 3).then(|| (parts[0] << 16) | (parts[1] << 8) | parts[2]);
    }
    let hex = text.strip_prefix('#').unwrap_or(text);
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    match hex.len() {
        6 => u32::from_str_radix(hex, 16).ok(),
        3 => {
            let short = u32::from_str_radix(hex, 16).ok()?;
            let (r, g, b) = ((short >> 8) & 0xf, (short >> 4) & 0xf, short & 0xf);
            Some(((r * 17) << 16) | ((g * 17) << 8) | (b * 17))
        }
        _ => None,
    }
}

/// The model people used last, for the next picker.
static LAST_MODEL: AtomicU8 = AtomicU8::new(0);

pub fn last_model() -> Model {
    Model::ALL[(LAST_MODEL.load(Ordering::Relaxed) as usize).min(2)]
}

pub fn remember_model(model: Model) {
    let at = Model::ALL.iter().position(|m| *m == model).unwrap_or(0);
    LAST_MODEL.store(at as u8, Ordering::Relaxed);
}

/// Colors chosen lately, newest first, while Feather runs.
static RECENT: Mutex<Vec<u32>> = Mutex::new(Vec::new());
pub const RECENT_MAX: usize = 12;

pub fn recent() -> Vec<u32> {
    RECENT.lock().map(|list| list.clone()).unwrap_or_default()
}

pub fn remember_color(rgb: u32) {
    if let Ok(mut list) = RECENT.lock() {
        list.retain(|other| *other != rgb);
        list.insert(0, rgb);
        list.truncate(RECENT_MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_round_trip() {
        for rgb in [0x5600d8, 0xf2a33a, 0x000000, 0xffffff, 0x3cb4c8, 0x808080] {
            for model in Model::ALL {
                let values = model.from_rgb(rgb, [0.0; 3]);
                let back = model.to_rgb(values);
                let (a, b) = (unpack(rgb), unpack(back));
                let off = (a.0 - b.0)
                    .abs()
                    .max((a.1 - b.1).abs())
                    .max((a.2 - b.2).abs());
                assert!(off <= 1.5 / 255.0, "{model:?} {rgb:06x} -> {back:06x}");
            }
        }
        // The reference purple: OKLCH about 44, 65, 286.
        let [l, c, h] = Model::Oklch.from_rgb(0x5600d8, [0.0; 3]);
        assert!(
            (l - 44.0).abs() < 1.5 && (h - 286.0).abs() < 2.0,
            "{l} {c} {h}"
        );
    }

    #[test]
    fn grays_keep_their_hue() {
        assert_eq!(
            Model::Oklch.from_rgb(0x808080, [50.0, 10.0, 123.0])[2],
            123.0
        );
        assert_eq!(Model::Hsl.from_rgb(0x808080, [77.0, 10.0, 50.0])[0], 77.0);
    }

    #[test]
    fn pasted_colors_are_read() {
        assert_eq!(parse("#5600D8"), Some(0x5600d8));
        assert_eq!(parse(" 5600d8 "), Some(0x5600d8));
        assert_eq!(parse("#fa0"), Some(0xffaa00));
        assert_eq!(parse("rgb(86, 0, 216)"), Some(0x5600d8));
        assert_eq!(parse("rgb(300, 0, 0)"), None);
        assert_eq!(parse("purple"), None);
    }
}

//! Time-based tweens for the main window: hover fades, the sliding selection
//! highlights, smooth scrolling, the details pane fade, and the theme
//! cross-fade.
//!
//! Values are computed from the clock when a frame is painted, so a slow
//! frame never slows an animation down; it only skips ahead. A 16 ms timer
//! repaints while anything is moving and stops when everything has settled.
//! When Windows has animation effects turned off, every change is instant.

use std::time::{Duration, Instant};

use super::layout::ROW_H;
use super::paint::Hit;
use super::theme::Palette;

pub const HOVER: Duration = Duration::from_millis(120);
pub const SCROLL: Duration = Duration::from_millis(180);
pub const SELECT: Duration = Duration::from_millis(170);
const NAV: Duration = Duration::from_millis(220);
const DETAILS: Duration = Duration::from_millis(200);
pub const THEME: Duration = Duration::from_millis(260);
/// Selection jumps farther than this many rows snap instead of sliding.
const MAX_SLIDE_ROWS: usize = 12;

/// Cubic ease-out: fast start, gentle landing.
pub fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// A value moving from `from` to `to` over `duration`.
#[derive(Clone, Copy, Debug)]
pub struct Tween {
    from: f32,
    to: f32,
    start: Instant,
    duration: Duration,
}

impl Default for Tween {
    fn default() -> Self {
        Self::new(0.0)
    }
}

impl Tween {
    pub fn new(value: f32) -> Self {
        Self {
            from: value,
            to: value,
            start: Instant::now(),
            duration: Duration::ZERO,
        }
    }

    fn progress(&self, now: Instant) -> f32 {
        if self.duration.is_zero() {
            return 1.0;
        }
        let elapsed = now.saturating_duration_since(self.start).as_secs_f32();
        (elapsed / self.duration.as_secs_f32()).clamp(0.0, 1.0)
    }

    pub fn value(&self, now: Instant) -> f32 {
        self.from + (self.to - self.from) * ease_out(self.progress(now))
    }

    pub fn target(&self) -> f32 {
        self.to
    }

    pub fn running(&self, now: Instant) -> bool {
        self.progress(now) < 1.0
    }

    /// Starts moving toward `to` from wherever the value is now.
    pub fn animate_to(&mut self, to: f32, duration: Duration, now: Instant) {
        if to == self.to {
            return;
        }
        self.from = self.value(now);
        self.to = to;
        self.start = now;
        self.duration = duration;
    }

    pub fn jump(&mut self, to: f32) {
        self.from = to;
        self.to = to;
        self.duration = Duration::ZERO;
    }

    pub fn settle(&mut self) {
        self.jump(self.to);
    }
}

/// A `Tween` in double precision, for the result list: with millions of
/// rows the offsets pass what `f32` can hold to the pixel.
#[derive(Clone, Copy, Debug)]
pub struct Tween64 {
    from: f64,
    to: f64,
    start: Instant,
    duration: Duration,
}

impl Tween64 {
    pub fn new(value: f64) -> Self {
        Self {
            from: value,
            to: value,
            start: Instant::now(),
            duration: Duration::ZERO,
        }
    }

    fn progress(&self, now: Instant) -> f32 {
        if self.duration.is_zero() {
            return 1.0;
        }
        let elapsed = now.saturating_duration_since(self.start).as_secs_f32();
        (elapsed / self.duration.as_secs_f32()).clamp(0.0, 1.0)
    }

    pub fn value(&self, now: Instant) -> f64 {
        let t = self.progress(now);
        if t >= 1.0 {
            return self.to;
        }
        self.from + (self.to - self.from) * f64::from(ease_out(t))
    }

    pub fn target(&self) -> f64 {
        self.to
    }

    pub fn running(&self, now: Instant) -> bool {
        self.progress(now) < 1.0
    }

    pub fn animate_to(&mut self, to: f64, duration: Duration, now: Instant) {
        if to == self.to {
            return;
        }
        self.from = self.value(now);
        self.to = to;
        self.start = now;
        self.duration = duration;
    }

    pub fn jump(&mut self, to: f64) {
        self.from = to;
        self.to = to;
        self.duration = Duration::ZERO;
    }

    pub fn settle(&mut self) {
        self.jump(self.to);
    }
}

/// Hover highlights that fade in and out, keyed by what is under the
/// pointer.
pub struct Fades<K> {
    items: Vec<(K, Tween)>,
}

impl<K> Default for Fades<K> {
    fn default() -> Self {
        Self { items: Vec::new() }
    }
}

impl<K: Copy + PartialEq> Fades<K> {
    /// Fades `old` out and `new` in.
    pub fn hover(&mut self, old: Option<K>, new: Option<K>, duration: Duration, now: Instant) {
        if let Some(old) = old {
            if let Some((_, tween)) = self.items.iter_mut().find(|(key, _)| *key == old) {
                tween.animate_to(0.0, duration, now);
            }
        }
        if let Some(new) = new {
            match self.items.iter_mut().find(|(key, _)| *key == new) {
                Some((_, tween)) => tween.animate_to(1.0, duration, now),
                None => {
                    let mut tween = Tween::new(0.0);
                    tween.animate_to(1.0, duration, now);
                    self.items.push((new, tween));
                }
            }
        }
    }

    pub fn level(&self, key: K, now: Instant) -> f32 {
        self.items
            .iter()
            .find(|(candidate, _)| *candidate == key)
            .map(|(_, tween)| tween.value(now))
            .unwrap_or(0.0)
    }

    pub fn running(&self, now: Instant) -> bool {
        self.items.iter().any(|(_, tween)| tween.running(now))
    }

    pub fn prune(&mut self, now: Instant) {
        self.items
            .retain(|(_, tween)| tween.running(now) || tween.target() > 0.0);
    }

    pub fn settle(&mut self) {
        for (_, tween) in &mut self.items {
            tween.settle();
        }
        self.items.retain(|(_, tween)| tween.target() > 0.0);
    }
}

/// Identifies one selected row: the row index within one set of results.
/// The epoch changes whenever the results are replaced or re-sorted.
pub type RowKey = (u64, usize);

pub struct Motion {
    /// Follows the Windows "Animation effects" setting.
    pub enabled: bool,
    hovers: Vec<(Hit, Tween)>,
    /// Target and displayed scroll offset of the result list.
    pub scroll: Tween64,
    /// Top of the selection highlight, in list content coordinates.
    row: Tween64,
    row_key: Option<RowKey>,
    /// Top of the selected sidebar entry in each group (kinds, places).
    nav: [Tween; 2],
    nav_key: [Option<usize>; 2],
    /// Fades the details pane in when the selection changes.
    details: Tween,
    theme: Tween,
    theme_from: Option<Palette>,
}

impl Default for Motion {
    fn default() -> Self {
        Self {
            enabled: true,
            hovers: Vec::new(),
            scroll: Tween64::new(0.0),
            row: Tween64::new(0.0),
            row_key: None,
            nav: [Tween::new(0.0), Tween::new(0.0)],
            nav_key: [None, None],
            details: Tween::new(1.0),
            theme: Tween::new(1.0),
            theme_from: None,
        }
    }
}

impl Motion {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            ..Self::default()
        }
    }

    fn time(&self, duration: Duration) -> Duration {
        if self.enabled {
            duration
        } else {
            Duration::ZERO
        }
    }

    /// Fades the old hover target out and the new one in.
    pub fn hover(&mut self, old: Hit, new: Hit, now: Instant) {
        let duration = self.time(HOVER);
        if old != Hit::None {
            if let Some((_, tween)) = self.hovers.iter_mut().find(|(hit, _)| *hit == old) {
                tween.animate_to(0.0, duration, now);
            }
        }
        if new != Hit::None {
            match self.hovers.iter_mut().find(|(hit, _)| *hit == new) {
                Some((_, tween)) => tween.animate_to(1.0, duration, now),
                None => {
                    let mut tween = Tween::new(0.0);
                    tween.animate_to(1.0, duration, now);
                    self.hovers.push((new, tween));
                }
            }
        }
    }

    /// How hovered each target is, from 0 to 1, for painting.
    pub fn hover_levels(&self, now: Instant) -> Vec<(Hit, f32)> {
        self.hovers
            .iter()
            .map(|(hit, tween)| (*hit, tween.value(now)))
            .filter(|(_, level)| *level > 0.001)
            .collect()
    }

    pub fn scroll_to(&mut self, target: f64, smooth: bool, now: Instant) {
        if smooth {
            let duration = self.time(SCROLL);
            self.scroll.animate_to(target, duration, now);
        } else {
            self.scroll.jump(target);
        }
    }

    /// Moves the selection highlight to a new row. Nearby rows slide; a new
    /// set of results or a long jump snaps.
    pub fn select_row(&mut self, key: Option<RowKey>, now: Instant) {
        if key == self.row_key {
            return;
        }
        if let Some((epoch, row)) = key {
            let y = row as f64 * f64::from(ROW_H);
            match self.row_key {
                Some((old_epoch, old_row))
                    if old_epoch == epoch && old_row.abs_diff(row) <= MAX_SLIDE_ROWS =>
                {
                    let duration = self.time(SELECT);
                    self.row.animate_to(y, duration, now);
                }
                _ => self.row.jump(y),
            }
            self.details.jump(0.0);
            let duration = self.time(DETAILS);
            self.details.animate_to(1.0, duration, now);
        }
        self.row_key = key;
    }

    pub fn row_top(&self, now: Instant) -> f64 {
        self.row.value(now)
    }

    /// Moves a sidebar group's highlight to the entry at `index`, whose top
    /// edge is at `y`.
    pub fn select_nav(&mut self, group: usize, index: Option<usize>, y: f32, now: Instant) {
        if self.nav_key[group] == index {
            return;
        }
        if self.nav_key[group].is_some() && index.is_some() {
            let duration = self.time(NAV);
            self.nav[group].animate_to(y, duration, now);
        } else {
            self.nav[group].jump(y);
        }
        self.nav_key[group] = index;
    }

    pub fn nav_top(&self, group: usize, now: Instant) -> f32 {
        self.nav[group].value(now)
    }

    /// Re-anchors a sidebar highlight after the window is resized or the
    /// entries move, without animating.
    pub fn reset_nav(&mut self) {
        self.nav_key = [None, None];
    }

    pub fn details_opacity(&self, now: Instant) -> f32 {
        self.details.value(now)
    }

    /// Cross-fades from the palette currently on screen.
    pub fn change_theme(&mut self, shown: Palette, now: Instant) {
        if !self.enabled {
            self.theme_from = None;
            return;
        }
        self.theme_from = Some(shown);
        self.theme.jump(0.0);
        self.theme.animate_to(1.0, THEME, now);
    }

    /// The palette to paint with while a theme change is fading in.
    pub fn palette(&self, target: Palette, now: Instant) -> Palette {
        match self.theme_from {
            Some(from) if self.theme.running(now) => {
                Palette::mix(&from, &target, self.theme.value(now))
            }
            _ => target,
        }
    }

    /// True while anything still needs frames.
    pub fn active(&self, now: Instant) -> bool {
        self.scroll.running(now)
            || self.row.running(now)
            || self.details.running(now)
            || self.theme.running(now)
            || self.nav.iter().any(|tween| tween.running(now))
            || self.hovers.iter().any(|(_, tween)| tween.running(now))
    }

    /// Drops hover entries that have fully faded out.
    pub fn prune(&mut self, now: Instant) {
        self.hovers
            .retain(|(_, tween)| tween.running(now) || tween.target() > 0.0);
        if !self.theme.running(now) {
            self.theme_from = None;
        }
    }

    /// Finishes every animation, for screenshots.
    pub fn settle(&mut self) {
        self.scroll.settle();
        self.row.settle();
        self.details.settle();
        self.theme.settle();
        self.theme_from = None;
        for tween in &mut self.nav {
            tween.settle();
        }
        for (_, tween) in &mut self.hovers {
            tween.settle();
        }
        self.hovers.retain(|(_, tween)| tween.target() > 0.0);
    }
}

/// Reads the Windows "Animation effects" setting (Settings > Accessibility >
/// Visual effects).
pub fn system_animations_enabled() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SystemParametersInfoW, SPI_GETCLIENTAREAANIMATION,
    };
    let mut enabled: i32 = 1;
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            &mut enabled as *mut i32 as *mut _,
            0,
        )
    };
    ok == 0 || enabled != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tweens_ease_toward_their_target() {
        let start = Instant::now();
        let mut tween = Tween::new(0.0);
        tween.animate_to(100.0, Duration::from_millis(100), start);
        assert_eq!(tween.value(start), 0.0);
        let half = tween.value(start + Duration::from_millis(50));
        assert!(
            half > 50.0 && half < 100.0,
            "ease-out is past halfway: {half}"
        );
        assert_eq!(tween.value(start + Duration::from_millis(150)), 100.0);
        assert!(!tween.running(start + Duration::from_millis(100)));
    }

    #[test]
    fn retargeting_starts_from_the_current_value() {
        let start = Instant::now();
        let mut tween = Tween::new(0.0);
        tween.animate_to(10.0, Duration::from_millis(100), start);
        let mid = start + Duration::from_millis(50);
        let at_mid = tween.value(mid);
        tween.animate_to(0.0, Duration::from_millis(100), mid);
        assert_eq!(tween.value(mid), at_mid);
    }

    #[test]
    fn selection_slides_nearby_and_snaps_far_away() {
        let now = Instant::now();
        let mut motion = Motion::default();
        motion.select_row(Some((1, 0)), now);
        assert_eq!(motion.row_top(now), 0.0);
        motion.select_row(Some((1, 3)), now);
        assert!(motion.active(now));
        assert_eq!(motion.row_top(now), 0.0);
        motion.select_row(Some((1, 500)), now);
        assert_eq!(motion.row_top(now), 500.0 * f64::from(ROW_H));
        motion.select_row(Some((2, 1)), now);
        assert_eq!(motion.row_top(now), f64::from(ROW_H), "new results snap");
    }

    #[test]
    fn disabled_motion_is_instant() {
        let now = Instant::now();
        let mut motion = Motion {
            enabled: false,
            ..Motion::default()
        };
        motion.hover(Hit::None, Hit::Search, now);
        motion.scroll_to(300.0, true, now);
        motion.select_row(Some((1, 0)), now);
        motion.select_row(Some((1, 2)), now);
        assert!(!motion.active(now));
        assert_eq!(motion.scroll.value(now), 300.0);
        assert_eq!(motion.hover_levels(now), vec![(Hit::Search, 1.0)]);
    }

    #[test]
    fn faded_out_hovers_are_pruned() {
        let now = Instant::now();
        let mut motion = Motion::default();
        motion.hover(Hit::None, Hit::Search, now);
        motion.hover(Hit::Search, Hit::None, now);
        let later = now + Duration::from_secs(1);
        motion.prune(later);
        assert!(motion.hover_levels(later).is_empty());
        assert!(!motion.active(later));
    }
}

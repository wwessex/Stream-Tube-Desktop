//! Aspect ratio and crop of the player picture, like the Ultrawidify browser
//! extension: crop the black bars of letterboxed films away (found in the frames
//! or from a fixed ratio) so they fill an ultrawide screen, zoom to fill, or stretch.

/// Choices for `Settings::aspect_mode`: (value, label).
pub const MODES: &[(&str, &str)] = &[
    ("original", "Original"),
    ("auto", "Auto (crop black bars)"),
    ("fill", "Zoom to fill"),
    ("stretch", "Stretch"),
    ("21:9", "21:9 (2.39:1)"),
    ("18:9", "18:9 (2:1)"),
    ("32:9", "32:9"),
];

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    /// the whole picture, fitted (letterboxed) as it comes
    Original,
    /// black bars found in the frames are cropped away
    Auto,
    /// fills the player, cropping what doesn't fit
    Fill,
    /// fills the player, distorting the picture
    Stretch,
    /// the picture holds a letterboxed film of this width:height
    Ratio(f32),
}

impl Mode {
    pub fn parse(s: &str) -> Mode {
        match s {
            "auto" => Mode::Auto,
            "fill" => Mode::Fill,
            "stretch" => Mode::Stretch,
            // cinemascope, what "21:9" films really are
            "21:9" => Mode::Ratio(2.39),
            "18:9" => Mode::Ratio(2.0),
            "32:9" => Mode::Ratio(32.0 / 9.0),
            _ => Mode::Original,
        }
    }

    pub fn without_zoom(self) -> Self {
        if self == Mode::Fill { Mode::Original } else { self }
    }
}

/// The setting after `cur` in `MODES`, for the shortcut that cycles through them.
pub fn next_mode(cur: &str) -> &'static str {
    let i = MODES.iter().position(|(v, _)| *v == cur).map(|i| i + 1).unwrap_or(0);
    MODES[i % MODES.len()].0
}

pub fn label(value: &str) -> &'static str {
    MODES.iter().find(|(v, _)| *v == value).map(|(_, l)| *l).unwrap_or(MODES[0].1)
}

/// Part of the frame, as fractions of its width and height.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Region {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Region {
    pub const FULL: Region = Region { x: 0.0, y: 0.0, w: 1.0, h: 1.0 };

    /// The rectangle occupied by this region within a positioned frame.
    pub fn in_frame(self, (x, y, w, h): (f32, f32, f32, f32)) -> (f32, f32, f32, f32) {
        (x + self.x * w, y + self.y * h, self.w * w, self.h * h)
    }

    /// The same part with `trim` (fractions of the frame) taken off each side.
    fn trimmed(self, trim: (f32, f32)) -> Region {
        let (l, r) = (self.x.max(trim.0), (self.x + self.w).min(1.0 - trim.0));
        let (t, b) = (self.y.max(trim.1), (self.y + self.h).min(1.0 - trim.1));
        if r <= l || b <= t {
            return self;
        }
        Region { x: l, y: t, w: r - l, h: b - t }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Fit {
    Contain,
    Cover,
    Stretch,
}

/// The video picture inside a rendered frame. mpv fits it, centered, in a frame
/// whose width is rounded up to a multiple of 16, so it can have thin side bars.
pub fn picture_region(frame: (f32, f32), video: (f32, f32)) -> Region {
    let ((fw, fh), (vw, vh)) = (frame, video);
    if fw <= 0.0 || fh <= 0.0 || vw <= 0.0 || vh <= 0.0 {
        return Region::FULL;
    }
    let k = (fw / vw).min(fh / vh);
    let (w, h) = ((vw * k / fw).min(1.0), (vh * k / fh).min(1.0));
    Region { x: (1.0 - w) / 2.0, y: (1.0 - h) / 2.0, w, h }
}

/// The part of the frame to show and how, for `mode`. `auto_trim` is what
/// [`AutoCrop`] found.
pub fn resolve(mode: Mode, frame: (f32, f32), video: (f32, f32), auto_trim: (f32, f32)) -> (Region, Fit) {
    let pic = picture_region(frame, video);
    match mode {
        Mode::Original => (pic, Fit::Contain),
        Mode::Fill => (pic, Fit::Cover),
        Mode::Stretch => (pic, Fit::Stretch),
        Mode::Auto => (pic.trimmed(auto_trim), Fit::Contain),
        Mode::Ratio(r) => {
            let aspect = pic.w * frame.0 / (pic.h * frame.1).max(1.0);
            // only letterboxing of landscape videos: a vertical Short in 21:9
            // mode would be cut down to a strip
            if aspect < 1.0 || r <= aspect {
                return (pic, Fit::Contain);
            }
            let h = pic.h * aspect / r;
            (Region { x: pic.x, y: pic.y + (pic.h - h) / 2.0, w: pic.w, h }, Fit::Contain)
        }
    }
}

/// Where to draw the whole frame (x, y, width, height, relative to `area`) so
/// that `region` of it is fitted to the area and centered in it.
pub fn layout(area: (f32, f32), frame: (f32, f32), region: Region, fit: Fit) -> (f32, f32, f32, f32) {
    let ((aw, ah), (fw, fh)) = (area, frame);
    let (cw, ch) = (region.w * fw, region.h * fh);
    if cw <= 0.0 || ch <= 0.0 {
        return (0.0, 0.0, aw, ah);
    }
    let (sx, sy) = match fit {
        Fit::Contain => {
            let k = (aw / cw).min(ah / ch);
            (k, k)
        }
        Fit::Cover => {
            let k = (aw / cw).max(ah / ch);
            (k, k)
        }
        Fit::Stretch => (aw / cw, ah / ch),
    };
    let (w, h) = (fw * sx, fh * sy);
    (aw / 2.0 - (region.x + region.w / 2.0) * w, ah / 2.0 - (region.y + region.h / 2.0) * h, w, h)
}

/// The brightest channel a pixel can have and still count as a black bar
/// (bars are 16 in limited-range video, plus compression noise).
const DARK: u8 = 28;
/// Points sampled along each row or column.
const SAMPLES: u32 = 48;
/// Bars wider than this (each side) are taken for a dark scene, not a letterbox.
const MAX_TRIM: f32 = 0.25;

/// Black bars in a BGRA frame: the fraction of its width cropped off the left and
/// right and of its height off the top and bottom (the smaller of the two sides,
/// so a dark edge of the picture isn't cut). None when it can't tell (a dark frame).
pub fn detect_bars(data: &[u8], w: u32, h: u32) -> Option<(f32, f32)> {
    if data.len() < (w as usize * h as usize * 4) {
        return None;
    }
    detect_bars_by(w, h, |x, y| {
        let i = (y as usize * w as usize + x as usize) * 4;
        is_dark([data[i], data[i + 1], data[i + 2]])
    })
}

/// Whether a pixel (any channel order) is dark enough to be part of a black bar.
pub fn is_dark(px: [u8; 3]) -> bool {
    px.iter().all(|&c| c <= DARK)
}

/// [`detect_bars`] for any `w`x`h` picture, `dark(x, y)` telling whether a pixel
/// counts as black. Used for frames that aren't BGRA bytes (macOS's GPU path).
pub fn detect_bars_by(w: u32, h: u32, dark: impl Fn(u32, u32) -> bool) -> Option<(f32, f32)> {
    if w < 16 || h < 16 {
        return None;
    }
    // a line is a bar when nearly all of it is black (a channel logo can sit in a bar)
    let row_dark = |y: u32| (0..SAMPLES).filter(|&i| dark((w - 1) * i / (SAMPLES - 1), y)).count() as u32 >= SAMPLES - 2;
    let col_dark = |x: u32| (0..SAMPLES).filter(|&i| dark(x, (h - 1) * i / (SAMPLES - 1))).count() as u32 >= SAMPLES - 2;
    let edge = |len: u32, is_dark: &dyn Fn(u32) -> bool| -> Option<u32> {
        let limit = (len as f32 * MAX_TRIM) as u32;
        let first = (0..=limit).find(|&i| !is_dark(i))?;
        let last = (0..=limit).find(|&i| !is_dark(len - 1 - i))?;
        Some(first.min(last))
    };
    let ty = edge(h, &row_dark)?;
    let tx = edge(w, &col_dark)?;
    Some((tx as f32 / w as f32, ty as f32 / h as f32))
}

/// Bars this close are the same bars.
const TOLERANCE: f32 = 0.006;
/// Detections in a row it takes before the picture zooms in further.
const CONFIRM: u8 = 3;

/// Black bars of the playing video, steadied over several frames.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AutoCrop {
    /// fractions of the frame cropped off each side (horizontally, vertically)
    pub trim: (f32, f32),
    candidate: Option<((f32, f32), u8)>,
}

impl AutoCrop {
    /// Takes one detection; true when the crop changed.
    pub fn feed(&mut self, detected: Option<(f32, f32)>) -> bool {
        let Some((dx, dy)) = detected else { return false };
        let snap = |v: f32| if v < 0.01 { 0.0 } else { (v * 400.0).floor() / 400.0 };
        let (dx, dy) = (snap(dx), snap(dy));
        let old = self.trim;
        // the picture reaching into the cropped part: show it at once
        if dx + TOLERANCE < self.trim.0 {
            self.trim.0 = dx;
        }
        if dy + TOLERANCE < self.trim.1 {
            self.trim.1 = dy;
        }
        // wider bars only once they've been seen a few times, so a dark
        // moment doesn't zoom in
        if dx > self.trim.0 + TOLERANCE || dy > self.trim.1 + TOLERANCE {
            let target = (dx.max(self.trim.0), dy.max(self.trim.1));
            let near = |a: (f32, f32)| (a.0 - target.0).abs() <= TOLERANCE && (a.1 - target.1).abs() <= TOLERANCE;
            let n = match self.candidate {
                Some((c, n)) if near(c) => n + 1,
                _ => 1,
            };
            if n >= CONFIRM {
                self.trim = target;
                self.candidate = None;
            } else {
                self.candidate = Some((target, n));
            }
        } else {
            self.candidate = None;
        }
        self.trim != old
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> bool {
        [(a.0, b.0), (a.1, b.1), (a.2, b.2), (a.3, b.3)].iter().all(|(x, y)| (x - y).abs() < 0.5)
    }

    /// A BGRA frame with black bars of `bar_y` rows top and bottom and `bar_x` columns left and right.
    fn frame(w: u32, h: u32, bar_x: u32, bar_y: u32, lum: u8) -> Vec<u8> {
        let mut d = vec![0u8; (w * h * 4) as usize];
        for y in bar_y..h - bar_y {
            for x in bar_x..w - bar_x {
                let i = ((y * w + x) * 4) as usize;
                d[i..i + 4].copy_from_slice(&[lum, lum, lum, 255]);
            }
        }
        d
    }

    #[test]
    fn original_fits_the_picture_like_before() {
        let (r, fit) = resolve(Mode::Original, (1920.0, 1080.0), (1920.0, 1080.0), (0.0, 0.0));
        // 16:9 in a 21:9 screen: pillarboxed
        assert!(close(layout((3440.0, 1440.0), (1920.0, 1080.0), r, fit), (440.0, 0.0, 2560.0, 1440.0)));
    }

    #[test]
    fn a_cinemascope_ratio_fills_an_ultrawide_screen() {
        let (r, fit) = resolve(Mode::Ratio(2.39), (1920.0, 1080.0), (1920.0, 1080.0), (0.0, 0.0));
        let (x, y, w, h) = layout((3440.0, 1440.0), (1920.0, 1080.0), r, fit);
        // the film's 2.39:1 band fills the 3440x1440 screen (to a pixel: 21:9 is 2.389:1)
        let (band_w, band_h) = (w * r.w, h * r.h);
        assert!((band_w - 3440.0).abs() < 1.0 && (band_h - 1440.0).abs() < 1.0, "{band_w} {band_h}");
        // the bars are drawn off screen
        assert!(x.abs() < 0.5 && y < -200.0, "{x} {y}");
    }

    #[test]
    fn ratios_never_crop_the_sides_of_tall_videos() {
        let (r, _) = resolve(Mode::Ratio(2.39), (608.0, 1080.0), (608.0, 1080.0), (0.0, 0.0));
        assert_eq!(r, Region::FULL);
    }

    #[test]
    fn fill_and_stretch_cover_the_player() {
        let area = (3440.0, 1440.0);
        let (r, fit) = resolve(Mode::Fill, (1920.0, 1080.0), (1920.0, 1080.0), (0.0, 0.0));
        let (x, y, w, h) = layout(area, (1920.0, 1080.0), r, fit);
        assert!(x <= 0.0 && y <= 0.0 && x + w >= area.0 && y + h >= area.1);
        assert!((w / h - 16.0 / 9.0).abs() < 0.01);
        let (r, fit) = resolve(Mode::Stretch, (1920.0, 1080.0), (1920.0, 1080.0), (0.0, 0.0));
        assert!(close(layout(area, (1920.0, 1080.0), r, fit), (0.0, 0.0, 3440.0, 1440.0)));
    }

    #[test]
    fn full_height_fits_each_aspect_mode() {
        let frame = (1920.0, 1080.0);
        let area = (1600.0, 1080.0);
        let trim = (0.0, 0.125);
        for &(value, _) in MODES {
            let mode = Mode::parse(value);
            let (region, _) = resolve(mode, frame, frame, trim);
            let (visible, fit) = resolve(mode.without_zoom(), frame, frame, trim);
            assert_eq!(visible, region, "{value}");
            let actual = visible.in_frame(layout(area, frame, visible, fit));
            let h = if mode == Mode::Stretch { area.1 } else { area.0 * (region.h * frame.1) / (region.w * frame.0) };
            assert!(close(actual, (0.0, (area.1 - h) / 2.0, area.0, h)), "{value}");
        }
    }

    #[test]
    fn the_padding_mpv_adds_to_the_frame_is_left_out() {
        // 854x480 drawn in an 864-wide frame
        let pic = picture_region((864.0, 480.0), (854.0, 480.0));
        assert!((pic.w * 864.0 - 854.0).abs() < 0.5 && (pic.x * 864.0 - 5.0).abs() < 0.5);
        let (x, _, w, _) = layout((854.0, 480.0), (864.0, 480.0), pic, Fit::Contain);
        assert!((x + 5.0).abs() < 0.5 && (w - 864.0).abs() < 0.5);
    }

    #[test]
    fn finds_letterbox_bars() {
        let (w, h) = (1920, 1080);
        let (tx, ty) = detect_bars(&frame(w, h, 0, 138, 120), w, h).unwrap();
        assert_eq!(tx, 0.0);
        assert!((ty - 138.0 / 1080.0).abs() < 0.001);
        let (tx, _) = detect_bars(&frame(w, h, 240, 0, 120), w, h).unwrap();
        assert!((tx - 240.0 / 1920.0).abs() < 0.001);
    }

    #[test]
    fn any_pixel_source_finds_the_same_bars() {
        // what macOS's GPU path does: no BGRA bytes, just a dark-pixel test
        let (w, h) = (1920u32, 1080u32);
        let d = frame(w, h, 0, 138, 120);
        let by_bytes = detect_bars(&d, w, h);
        let by_pixel = detect_bars_by(w, h, |_, y| y < 138 || y >= h - 138);
        assert_eq!(by_bytes, by_pixel);
        assert_eq!(detect_bars_by(8, 8, |_, _| false), None);
    }

    #[test]
    fn a_dark_frame_is_not_a_letterbox() {
        let (w, h) = (640, 360);
        assert_eq!(detect_bars(&frame(w, h, 0, 0, 0), w, h), None);
        // bars this wide are a dark scene with something lit in the middle
        assert_eq!(detect_bars(&frame(w, h, 0, 120, 200), w, h), None);
    }

    #[test]
    fn a_logo_in_the_bar_does_not_hide_it() {
        let (w, h) = (1920u32, 1080u32);
        let mut d = frame(w, h, 0, 138, 120);
        // a small bright logo in the top bar
        for y in 20..60 {
            for x in 1700..1760 {
                let i = ((y * w + x) * 4) as usize;
                d[i..i + 3].copy_from_slice(&[255, 255, 255]);
            }
        }
        let (_, ty) = detect_bars(&d, w, h).unwrap();
        assert!((ty - 138.0 / 1080.0).abs() < 0.001);
    }

    #[test]
    fn zooms_in_only_on_steady_bars_and_out_at_once() {
        let mut a = AutoCrop::default();
        assert!(!a.feed(Some((0.0, 0.128))));
        assert!(!a.feed(Some((0.0, 0.127))));
        assert!(a.feed(Some((0.0, 0.128))));
        assert!((a.trim.1 - 0.1275).abs() < 0.003);
        // a dark moment doesn't change it
        assert!(!a.feed(None));
        assert!(!a.feed(Some((0.0, 0.2))));
        assert!(!a.feed(Some((0.0, 0.128))));
        // the picture filling the frame again shows it whole straight away
        assert!(a.feed(Some((0.0, 0.0))));
        assert_eq!(a.trim, (0.0, 0.0));
    }

    #[test]
    fn tiny_bars_are_ignored() {
        let mut a = AutoCrop::default();
        for _ in 0..5 {
            a.feed(Some((0.004, 0.006)));
        }
        assert_eq!(a.trim, (0.0, 0.0));
    }

    #[test]
    fn modes_cycle_and_unknown_values_are_original() {
        assert_eq!(next_mode("original"), "auto");
        assert_eq!(next_mode("32:9"), "original");
        assert_eq!(next_mode("bogus"), "original");
        assert_eq!(Mode::parse("bogus"), Mode::Original);
        assert_eq!(Mode::parse("21:9"), Mode::Ratio(2.39));
    }
}

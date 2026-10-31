//! Window layout presets and geometry computation (logical pixels, top-left origin).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Layout {
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    Center,
    Maximize,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Copy)]
enum Align {
    Start,
    Center,
    End,
}

fn align(start: f64, avail: f64, size: f64, a: Align) -> f64 {
    match a {
        Align::Start => start,
        Align::Center => start + (avail - size) / 2.0,
        Align::End => start + avail - size,
    }
}

impl Layout {
    /// Region of the work area this preset fills, and the alignment used when an
    /// explicit size is given.
    fn region(self, wa: Rect) -> (Rect, Align, Align) {
        let (hw, hh) = (wa.width / 2.0, wa.height / 2.0);
        let r = |x, y, width, height| Rect {
            x,
            y,
            width,
            height,
        };
        use Align::*;
        match self {
            Layout::Left => (r(wa.x, wa.y, hw, wa.height), Start, Center),
            Layout::Right => (r(wa.x + hw, wa.y, hw, wa.height), End, Center),
            Layout::Top => (r(wa.x, wa.y, wa.width, hh), Center, Start),
            Layout::Bottom => (r(wa.x, wa.y + hh, wa.width, hh), Center, End),
            Layout::TopLeft => (r(wa.x, wa.y, hw, hh), Start, Start),
            Layout::TopRight => (r(wa.x + hw, wa.y, hw, hh), End, Start),
            Layout::BottomLeft => (r(wa.x, wa.y + hh, hw, hh), Start, End),
            Layout::BottomRight => (r(wa.x + hw, wa.y + hh, hw, hh), End, End),
            Layout::Center | Layout::Maximize => (wa, Center, Center),
        }
    }
}

/// Computes the outer window frame for a layout preset.
///
/// * `content_size` — requested video area size; when given, the window keeps that
///   size and is anchored inside the work area according to the preset.
/// * `current_outer` — current outer size, used by `Center` without explicit size.
/// * `decorations` — outer minus inner size (title bar etc.).
pub fn compute(
    layout: Layout,
    work_area: Rect,
    content_size: Option<(f64, f64)>,
    current_outer: (f64, f64),
    decorations: (f64, f64),
) -> Rect {
    let (region, ha, va) = layout.region(work_area);
    let (w, h) = match (content_size, layout) {
        (Some((w, h)), _) => (w + decorations.0, h + decorations.1),
        (None, Layout::Center) => current_outer,
        (None, _) => return region,
    };
    let (w, h) = (w.min(work_area.width), h.min(work_area.height));
    Rect {
        x: align(work_area.x, work_area.width, w, ha),
        y: align(work_area.y, work_area.height, h, va),
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WA: Rect = Rect {
        x: 0.0,
        y: 25.0,
        width: 1600.0,
        height: 1000.0,
    };

    #[test]
    fn fills_regions() {
        let r = compute(Layout::Right, WA, None, (800.0, 600.0), (0.0, 28.0));
        assert_eq!(
            r,
            Rect {
                x: 800.0,
                y: 25.0,
                width: 800.0,
                height: 1000.0
            }
        );
        let r = compute(Layout::BottomLeft, WA, None, (800.0, 600.0), (0.0, 28.0));
        assert_eq!(
            r,
            Rect {
                x: 0.0,
                y: 525.0,
                width: 800.0,
                height: 500.0
            }
        );
        assert_eq!(
            compute(Layout::Maximize, WA, None, (1.0, 1.0), (0.0, 0.0)),
            WA
        );
    }

    #[test]
    fn anchors_explicit_size() {
        let r = compute(
            Layout::BottomRight,
            WA,
            Some((640.0, 360.0)),
            (0.0, 0.0),
            (0.0, 28.0),
        );
        assert_eq!(
            r,
            Rect {
                x: 960.0,
                y: 637.0,
                width: 640.0,
                height: 388.0
            }
        );
    }

    #[test]
    fn centers_current_size() {
        let r = compute(Layout::Center, WA, None, (800.0, 600.0), (0.0, 28.0));
        assert_eq!(
            r,
            Rect {
                x: 400.0,
                y: 225.0,
                width: 800.0,
                height: 600.0
            }
        );
    }

    #[test]
    fn every_region_preset() {
        let d = (0.0, 0.0);
        let c = |l| compute(l, WA, None, (100.0, 100.0), d);
        let r = |x, y, width, height| Rect {
            x,
            y,
            width,
            height,
        };
        assert_eq!(c(Layout::Left), r(0.0, 25.0, 800.0, 1000.0));
        assert_eq!(c(Layout::Right), r(800.0, 25.0, 800.0, 1000.0));
        assert_eq!(c(Layout::Top), r(0.0, 25.0, 1600.0, 500.0));
        assert_eq!(c(Layout::Bottom), r(0.0, 525.0, 1600.0, 500.0));
        assert_eq!(c(Layout::TopLeft), r(0.0, 25.0, 800.0, 500.0));
        assert_eq!(c(Layout::TopRight), r(800.0, 25.0, 800.0, 500.0));
        assert_eq!(c(Layout::BottomLeft), r(0.0, 525.0, 800.0, 500.0));
        assert_eq!(c(Layout::BottomRight), r(800.0, 525.0, 800.0, 500.0));
        assert_eq!(c(Layout::Maximize), WA);
        assert_eq!(c(Layout::Center), r(750.0, 475.0, 100.0, 100.0));
    }

    #[test]
    fn every_anchor_with_explicit_size() {
        let a = |l| compute(l, WA, Some((400.0, 200.0)), (0.0, 0.0), (0.0, 0.0));
        let at = |x, y| Rect {
            x,
            y,
            width: 400.0,
            height: 200.0,
        };
        assert_eq!(a(Layout::Left), at(0.0, 425.0));
        assert_eq!(a(Layout::Right), at(1200.0, 425.0));
        assert_eq!(a(Layout::Top), at(600.0, 25.0));
        assert_eq!(a(Layout::Bottom), at(600.0, 825.0));
        assert_eq!(a(Layout::TopLeft), at(0.0, 25.0));
        assert_eq!(a(Layout::TopRight), at(1200.0, 25.0));
        assert_eq!(a(Layout::BottomLeft), at(0.0, 825.0));
        assert_eq!(a(Layout::BottomRight), at(1200.0, 825.0));
        assert_eq!(a(Layout::Center), at(600.0, 425.0));
        assert_eq!(a(Layout::Maximize), at(600.0, 425.0));
    }

    #[test]
    fn oversized_windows_are_clamped_to_the_work_area() {
        let r = compute(
            Layout::TopLeft,
            WA,
            Some((5000.0, 5000.0)),
            (0.0, 0.0),
            (0.0, 28.0),
        );
        assert_eq!(r, WA);
    }

    #[test]
    fn non_zero_work_area_origin() {
        let wa = Rect {
            x: -1920.0,
            y: 100.0,
            width: 1920.0,
            height: 1080.0,
        };
        let r = compute(Layout::Right, wa, None, (0.0, 0.0), (0.0, 0.0));
        assert_eq!(
            r,
            Rect {
                x: -960.0,
                y: 100.0,
                width: 960.0,
                height: 1080.0
            }
        );
    }
}

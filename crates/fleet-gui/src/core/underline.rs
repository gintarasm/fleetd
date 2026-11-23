//! Underlines gpui's text runs cannot draw (decision 0096): double, dotted
//! and dashed, as rectangles the shell fills. A single underline and an
//! undercurl are drawn with the text (gpui's straight and wavy underline).
//!
//! Lengths are in points, relative to the left end of a span of cells and
//! the top of its row.

use fleet_term::Underline;

/// A rectangle to fill.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mark {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// The part of a cell a dash covers; the rest is the gap before the next.
pub const DASH: f32 = 0.6;

/// The text run draws this underline itself (or there is none).
pub fn drawn_with_text(kind: Underline) -> bool {
    matches!(kind, Underline::None | Underline::Single | Underline::Curly)
}

/// The marks of a `kind` underline under `cells` cells `cell_w` wide, its
/// first line's top at `y` in a row `line_h` tall, lines `t` thick. Empty
/// for the kinds [`drawn_with_text`]. A double underline's second line goes
/// below the first, or above it when the row has no room.
pub fn marks(kind: Underline, cells: u16, cell_w: f32, y: f32, line_h: f32, t: f32) -> Vec<Mark> {
    let width = cell_w * f32::from(cells);
    let y = y.min(line_h - t).max(0.0);
    match kind {
        Underline::None | Underline::Single | Underline::Curly => Vec::new(),
        Underline::Double => {
            let below = y + 2.0 * t;
            let second = if below + t <= line_h {
                below
            } else {
                y - 2.0 * t
            };
            vec![
                Mark {
                    x: 0.0,
                    y,
                    w: width,
                    h: t,
                },
                Mark {
                    x: 0.0,
                    y: second.max(0.0),
                    w: width,
                    h: t,
                },
            ]
        }
        Underline::Dashed => (0..cells)
            .map(|c| Mark {
                x: cell_w * f32::from(c),
                y,
                w: cell_w * DASH,
                h: t,
            })
            .collect(),
        Underline::Dotted => {
            let step = 2.0 * t;
            let mut out = Vec::with_capacity((width / step) as usize + 1);
            let mut x = 0.0;
            while x + t <= width + f32::EPSILON {
                out.push(Mark { x, y, w: t, h: t });
                x += step;
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_and_curly_are_left_to_the_text_run() {
        for kind in [Underline::None, Underline::Single, Underline::Curly] {
            assert!(drawn_with_text(kind));
            assert!(marks(kind, 4, 8.0, 14.0, 17.0, 1.0).is_empty());
        }
        for kind in [Underline::Double, Underline::Dotted, Underline::Dashed] {
            assert!(!drawn_with_text(kind));
        }
    }

    #[test]
    fn double_is_two_full_width_lines_a_gap_apart() {
        let m = marks(Underline::Double, 3, 8.0, 13.0, 17.0, 1.0);
        assert_eq!(
            m,
            [
                Mark {
                    x: 0.0,
                    y: 13.0,
                    w: 24.0,
                    h: 1.0
                },
                Mark {
                    x: 0.0,
                    y: 15.0,
                    w: 24.0,
                    h: 1.0
                },
            ]
        );
    }

    #[test]
    fn double_goes_above_when_the_row_has_no_room_below() {
        let m = marks(Underline::Double, 1, 8.0, 15.5, 17.0, 1.0);
        assert_eq!(m[0].y, 15.5);
        assert_eq!(m[1].y, 13.5);
        assert!(m.iter().all(|r| r.y + r.h <= 17.0));
    }

    #[test]
    fn dashed_has_one_dash_per_cell() {
        let m = marks(Underline::Dashed, 3, 10.0, 14.0, 17.0, 1.0);
        let xs: Vec<f32> = m.iter().map(|r| r.x).collect();
        assert_eq!(xs, [0.0, 10.0, 20.0]);
        assert!(m.iter().all(|r| r.w == 6.0 && r.y == 14.0));
    }

    #[test]
    fn dotted_dots_are_a_thickness_apart_and_stay_inside_the_span() {
        let m = marks(Underline::Dotted, 2, 8.0, 14.0, 17.0, 1.0);
        let xs: Vec<f32> = m.iter().map(|r| r.x).collect();
        assert_eq!(xs, [0.0, 2.0, 4.0, 6.0, 8.0, 10.0, 12.0, 14.0]);
        assert!(m.iter().all(|r| r.w == 1.0 && r.h == 1.0));
    }

    #[test]
    fn the_line_never_falls_below_the_row() {
        let m = marks(Underline::Dashed, 1, 8.0, 20.0, 17.0, 1.0);
        assert_eq!(m[0].y, 16.0);
    }
}

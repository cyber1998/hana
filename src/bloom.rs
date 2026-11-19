//! A small flower that grows in the corner after a cleanup: seed, sprout, bud, bloom, a little
//! twinkle, then it fades. Pure frames here; tui.rs draws them in the theme's colors.

use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use crate::themes::Theme;

pub const WIDTH: u16 = 9;
pub const HEIGHT: u16 = 5;

/// How long the whole thing lasts.
pub const LENGTH: Duration = Duration::from_millis(3800);

/// Each frame is HEIGHT rows of WIDTH columns; spaces are see-through.
const SEED: [&str; 5] = ["         ", "         ", "         ", "    .    ", "   ___   "];
const SPROUT: [&str; 5] = ["         ", "         ", "         ", "    |    ", "   _|_   "];
const LEAF: [&str; 5] = ["         ", "         ", "    |    ", "    |/   ", "   _|_   "];
const BUD: [&str; 5] = ["         ", "    o    ", "    |    ", "   \\|/   ", "   _|_   "];
const OPENING: [&str; 5] = ["         ", "   (o)   ", "    |    ", "   \\|/   ", "   _|_   "];
const BLOOM: [&str; 5] = ["   .-.   ", "  ( ✿ )  ", "   '|'   ", "   \\|/   ", "   _|_   "];
const TWINKLE_A: [&str; 5] = [" * .-.   ", "  ( ✿ ) +", "   '|'   ", " + \\|/   ", "   _|_ * "];
const TWINKLE_B: [&str; 5] = ["   .-. + ", "* ( ✿ )  ", "   '|' * ", "   \\|/ + ", " * _|_   "];

/// The frame to show `elapsed` into the animation, and whether it is fading. None once it is over.
pub fn frame(elapsed: Duration) -> Option<(&'static [&'static str; 5], bool)> {
    let ms = elapsed.as_millis();
    let rows = match ms {
        0..=139 => &SEED,
        140..=279 => &SPROUT,
        280..=439 => &LEAF,
        440..=619 => &BUD,
        620..=799 => &OPENING,
        800..=1099 => &BLOOM,
        1100..=3199 => {
            if (ms / 260) % 2 == 0 {
                &TWINKLE_A
            } else {
                &TWINKLE_B
            }
        }
        3200..=3799 => &BLOOM,
        _ => return None,
    };
    Some((rows, ms >= 3200))
}

/// Draws the frame for `elapsed` with its bottom-right corner at the bottom-right of `area`.
/// Only the flower's own characters are written, so what is underneath stays readable.
pub fn draw(buf: &mut Buffer, area: Rect, elapsed: Duration, t: &Theme) {
    let Some((rows, fading)) = frame(elapsed) else { return };
    if area.width < WIDTH + 1 || area.height < HEIGHT {
        return;
    }
    let left = area.right() - WIDTH - 1;
    let top = area.bottom() - HEIGHT;
    for (dy, row) in rows.iter().enumerate() {
        for (dx, ch) in row.chars().enumerate() {
            if ch == ' ' {
                continue;
            }
            let color = match ch {
                '✿' | 'o' => t.warn,
                '.' if dy == 3 => t.good,
                '(' | ')' | '.' | '-' | '\'' if dy <= 2 => t.accent,
                '*' | '+' => t.info,
                _ => t.good,
            };
            let mut style = Style::new().fg(if fading { t.dim } else { color });
            if !fading && matches!(ch, '✿' | '(' | ')') {
                style = style.add_modifier(Modifier::BOLD);
            }
            if let Some(cell) = buf.cell_mut((left + dx as u16, top + dy as u16)) {
                cell.set_char(ch).set_style(style);
                if let Some(bg) = t.bg {
                    cell.set_bg(bg);
                }
            }
        }
    }
}

#[cfg(test)]
fn all_frames() -> [&'static [&'static str; 5]; 8] {
    [&SEED, &SPROUT, &LEAF, &BUD, &OPENING, &BLOOM, &TWINKLE_A, &TWINKLE_B]
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn frames_line_up() {
        for f in all_frames() {
            for row in f.iter() {
                assert_eq!(row.width(), WIDTH as usize, "{row:?}");
            }
        }
    }

    #[test]
    fn grows_then_ends() {
        assert!(std::ptr::eq(frame(Duration::ZERO).unwrap().0, &SEED));
        assert!(std::ptr::eq(frame(Duration::from_millis(900)).unwrap().0, &BLOOM));
        assert!(frame(Duration::from_millis(3500)).unwrap().1, "fades at the end");
        assert!(frame(LENGTH).is_none());
    }
}

//! The moment before hana opens: a cherry blossom grows and turns open in the middle of the
//! screen, then its name appears. Drawn in half-block "pixels" (two per terminal cell, one above
//! the other) so the flower reads as a picture rather than as letters. Any key skips it.

use std::f32::consts::PI;
use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::UnicodeWidthStr;

use crate::themes::Theme;

/// How long the intro lasts if no key is pressed.
pub const LENGTH: Duration = Duration::from_millis(1700);

/// The flower's size in pixels: SIZE columns, SIZE / 2 terminal rows.
const SIZE: usize = 34;
/// How long the flower takes to open; the name follows.
const GROW_MS: f32 = 800.0;

/// One pixel of the flower, from the outside in.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Pixel {
    Empty,
    Petal,
    /// The lighter inner part of each petal.
    Inner,
    /// A petal's rim, darker, so the five petals read apart.
    Edge,
    Center,
    Dot,
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

/// The flower as pixels, `open` from 0 (a bud) to 1 (in full bloom). Five petals, each an
/// ellipse pointing outward with the small notch at its tip that cherry blossom petals have.
pub fn flower(open: f32) -> [[Pixel; SIZE]; SIZE] {
    let mut out = [[Pixel::Empty; SIZE]; SIZE];
    let open = ease_out(open);
    let r = (SIZE as f32 / 2.0 - 0.5) * (0.25 + 0.75 * open);
    // Petals turn into place as they open, ending with one pointing straight up.
    let turn = -PI / 2.0 - (1.0 - open) * 0.9;
    let c = (SIZE as f32 - 1.0) / 2.0;
    // Which petal each pixel belongs to, for drawing their rims.
    let mut petal_of = [[usize::MAX; SIZE]; SIZE];
    for (y, row) in out.iter_mut().enumerate() {
        for (x, pixel) in row.iter_mut().enumerate() {
            let (dx, dy) = (x as f32 - c, y as f32 - c);
            let dist = (dx * dx + dy * dy).sqrt();
            if dist <= r * 0.08 {
                *pixel = Pixel::Dot;
                continue;
            }
            if dist <= r * 0.17 {
                *pixel = Pixel::Center;
                continue;
            }
            for k in 0..5 {
                let a = turn + k as f32 * 2.0 * PI / 5.0;
                // Along the petal (u) and across it (v).
                let u = dx * a.cos() + dy * a.sin();
                let v = -dx * a.sin() + dy * a.cos();
                let (half_len, half_width) = (r * 0.5, r * 0.4);
                let inside = ((u - half_len) / half_len).powi(2) + (v / half_width).powi(2) <= 1.0;
                let notch = (u - r * 1.02).powi(2) + v.powi(2) <= (r * 0.12).powi(2);
                if inside && !notch {
                    *pixel = if u < r * 0.36 { Pixel::Inner } else { Pixel::Petal };
                    petal_of[y][x] = k;
                    break;
                }
            }
        }
    }
    // A petal pixel next to the background or to another petal is its rim.
    let snapshot = out;
    for y in 0..SIZE {
        for x in 0..SIZE {
            if !matches!(snapshot[y][x], Pixel::Petal | Pixel::Inner) {
                continue;
            }
            let mine = petal_of[y][x];
            let neighbours = [(x.wrapping_sub(1), y), (x + 1, y), (x, y.wrapping_sub(1)), (x, y + 1)];
            let rim = neighbours.iter().any(|&(nx, ny)| match snapshot.get(ny).and_then(|r| r.get(nx)) {
                None | Some(Pixel::Empty) => true,
                Some(Pixel::Petal | Pixel::Inner | Pixel::Edge) => petal_of[ny][nx] != mine,
                Some(_) => false,
            });
            if rim {
                out[y][x] = Pixel::Edge;
            }
        }
    }
    out
}

/// `a` moved `f` of the way toward `b`, for colors given as RGB; otherwise `a`.
fn blend(a: Color, b: Color, f: f32) -> Color {
    match (a, b) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * f).round() as u8;
            Color::Rgb(mix(r1, r2), mix(g1, g2), mix(b1, b2))
        }
        _ => a,
    }
}

fn color_of(p: Pixel, t: &Theme) -> Option<Color> {
    match p {
        Pixel::Empty => None,
        Pixel::Petal => Some(t.accent),
        Pixel::Inner => Some(blend(t.accent, t.text, 0.45)),
        Pixel::Edge => Some(blend(t.accent, t.bg.unwrap_or(Color::Rgb(0, 0, 0)), 0.3)),
        Pixel::Center => Some(t.warn),
        Pixel::Dot => Some(blend(t.warn, t.accent, 0.6)),
    }
}

/// Draws the intro `elapsed` into it, centered in `area`.
pub fn draw(buf: &mut Buffer, area: Rect, elapsed: Duration, t: &Theme) {
    let ms = elapsed.as_millis() as f32;
    let pixels = flower(ms / GROW_MS);
    let rows = (SIZE / 2) as u16;
    // The flower, a blank line, the name and the tagline.
    let height = rows + 4;
    if area.width < SIZE as u16 || area.height < height {
        return;
    }
    let left = area.x + (area.width - SIZE as u16) / 2;
    let top = area.y + (area.height - height) / 2;
    let bg = t.bg.unwrap_or(Color::Reset);
    for row in 0..rows as usize {
        for x in 0..SIZE {
            let (upper, lower) = (color_of(pixels[row * 2][x], t), color_of(pixels[row * 2 + 1][x], t));
            let Some(cell) = buf.cell_mut((left + x as u16, top + row as u16)) else { continue };
            match (upper, lower) {
                (None, None) => continue,
                (Some(u), Some(l)) => cell.set_char('▀').set_style(Style::new().fg(u).bg(l)),
                (Some(u), None) => cell.set_char('▀').set_style(Style::new().fg(u).bg(bg)),
                (None, Some(l)) => cell.set_char('▄').set_style(Style::new().fg(l).bg(bg)),
            };
        }
    }

    // The name comes in once the flower is nearly open, the tagline a moment later.
    let line = |buf: &mut Buffer, y: u16, text: &str, style: Style| {
        let x = area.x + area.width.saturating_sub(text.width() as u16) / 2;
        buf.set_string(x, y, text, style.bg(bg));
    };
    if ms >= GROW_MS * 0.75 {
        line(buf, top + rows + 1, "h a n a", Style::new().fg(t.accent).add_modifier(Modifier::BOLD));
    }
    if ms >= GROW_MS {
        line(buf, top + rows + 2, "花 · cleans up after Claude Code", Style::new().fg(t.dim));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_into_a_five_petal_flower() {
        let bud = flower(0.0);
        let full = flower(1.0);
        let count = |f: &[[Pixel; SIZE]; SIZE], p: Pixel| f.iter().flatten().filter(|&&x| x == p).count();
        assert!(count(&full, Pixel::Petal) > count(&bud, Pixel::Petal) * 3, "it grows");
        assert!(count(&full, Pixel::Dot) > 0 && count(&full, Pixel::Center) > 0);
        // One petal points straight up once open: the top middle has petal, the corners do not.
        let c = SIZE / 2;
        assert!((1..4).any(|y| full[y][c] != Pixel::Empty));
        assert_eq!(full[0][0], Pixel::Empty);
        assert_eq!(full[SIZE - 1][SIZE - 1], Pixel::Empty);
    }
}

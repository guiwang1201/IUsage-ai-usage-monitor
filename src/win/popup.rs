//! Layout and drawing of the detail panel. The window itself is managed in `mod.rs`.

use windows_sys::Win32::Foundation::RECT;

use super::render::{draw_text, Align, Color, Font, Surface};
use super::{badge, fill_color};
use crate::model::Provider;
use crate::panel::{Panel, Section};

// Layout in 96-dpi pixels.
const WIDTH: f32 = 340.0;
const PAD: f32 = 16.0;
const TOP: f32 = 14.0;
const HEADER_H: f32 = 22.0;
const HEADER_GAP: f32 = 6.0;
const LINE_H: f32 = 18.0;
const BAR_GAP: f32 = 4.0;
const BAR_H: f32 = 6.0;
const DETAIL_GAP: f32 = 4.0;
const DETAIL_H: f32 = 16.0;
const ROW_GAP: f32 = 10.0;
const STATUS_GAP: f32 = 8.0;
const STATUS_H: f32 = 16.0;
const PACE_GAP: f32 = 14.0;
const PACE_H: f32 = 22.0;
const SECTION_GAP: f32 = 12.0;

const ROW_H: f32 = LINE_H + BAR_GAP + BAR_H + DETAIL_GAP + DETAIL_H;
const SECTION_H: f32 = HEADER_H + HEADER_GAP + ROW_H * 2.0 + ROW_GAP + STATUS_GAP + STATUS_H * 4.0 + PACE_GAP + PACE_H;
/// A provider with nothing to show: its header, a notice and an action line.
const NOTICE_H: f32 = HEADER_H + HEADER_GAP + STATUS_H * 2.0;
/// The gaps and the rule between two sections.
const DIVIDER_H: f32 = SECTION_GAP * 2.0 + 1.0;

const WARNING_TEXT: Color = Color::rgb(245, 158, 11);
const LINK_TEXT: Color = Color::rgb(45, 130, 225);

fn section_h(section: &Section) -> f32 {
    if section.unsubscribed { NOTICE_H } else { SECTION_H }
}

/// The clickable last line of a section, if it has one.
fn link(section: &Section) -> Option<&'static str> {
    if section.update_login {
        Some("更新网页登录…  （点击或按 Enter）")
    } else if section.unsubscribed && section.retry.is_empty() {
        Some("已续订？点此重新检查")
    } else {
        None
    }
}

/// Whose link is at (x, y), in physical pixels.
pub fn link_hit(panel: &Panel, x: i32, y: i32, scale: f32) -> Option<Provider> {
    let (x, y) = (x as f32 / scale, y as f32 / scale);
    if !(PAD..WIDTH - PAD).contains(&x) {
        return None;
    }
    let mut top = TOP;
    for section in panel {
        let bottom = top + section_h(section);
        if link(section).is_some() && (bottom - STATUS_H..bottom).contains(&y) {
            return Some(section.provider);
        }
        top = bottom + DIVIDER_H;
    }
    None
}

/// Window size in physical pixels.
pub fn size(scale: f32, panel: &Panel) -> (i32, i32) {
    let height = TOP * 2.0 + panel.iter().map(section_h).sum::<f32>() + DIVIDER_H * (panel.len() - 1) as f32;
    ((WIDTH * scale).round() as i32, (height * scale).round() as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Entry;
    use crate::model::{FetchError, Provider, Thresholds, Usage, Window};
    use crate::panel::{self, Context};

    /// Uses synthetic readings only. Optional BMP output lets a developer inspect
    /// both themes without starting the application's network polling loop.
    #[test]
    fn offline_panel_rendering() {
        let now = 1_790_860_000;
        let cache = [Some(Entry {
            source: crate::cache::Source::Web,
            usage: Usage {
                five_hour: Some(Window { used_pct: 23.0, resets_at: Some(now + 15_800) }),
                seven_day: Some(Window { used_pct: 91.0, resets_at: Some(now + 248_875) }),
            },
            fetched_at: now - 200,
            announced: [0, 0],
        }); 2];
        let errors = [Some(FetchError::WebUnauthorized), Some(FetchError::RateLimited(Some(600)))];
        let mut panel = panel::build(&cache, &errors, &Context {
            current: Provider::Claude, show_remaining: false, thresholds: Thresholds::default(), now, utc_offset: 8 * 3600,
        });
        panel[1].retry = "等待至 明天 16:30 · 23 小时 59 分钟".into();
        panel[0].retry = "已暂停请求，更新登录后恢复".into();
        let x = |scale: f32| (PAD * scale + 4.0) as i32;
        for scale in [1.0, 1.75] {
            let y = ((TOP + SECTION_H - STATUS_H / 2.0) * scale) as i32;
            assert_eq!(link_hit(&panel, x(scale), y, scale), Some(Provider::Claude));
            assert_eq!(link_hit(&panel, 0, y, scale), None);
            assert_eq!(link_hit(&panel, x(scale), 0, scale), None);
        }
        // Codex on a free plan shrinks to a notice whose last line asks for a recheck.
        let mut free = panel.clone();
        free[1] = panel::build(&[None, None], &[None, Some(FetchError::Unsubscribed)], &Context {
            current: Provider::Claude, show_remaining: false, thresholds: Thresholds::default(), now, utc_offset: 8 * 3600,
        })[1].clone();
        assert!(size(1.0, &free).1 < size(1.0, &panel).1);
        let codex_link = TOP + SECTION_H + DIVIDER_H + NOTICE_H - STATUS_H / 2.0;
        assert_eq!(link_hit(&free, x(1.0), codex_link as i32, 1.0), Some(Provider::Codex));
        assert_eq!(link_hit(&free, x(1.0), (codex_link - STATUS_H) as i32, 1.0), None);
        free[1].retry = "正在重新检查…".into();
        assert_eq!(link_hit(&free, x(1.0), codex_link as i32, 1.0), None, "no second click while one is pending");
        // Fresh readings add the pace line under the provenance.
        let ok = panel::build(&cache, &[None, None], &Context {
            current: Provider::Claude, show_remaining: true, thresholds: Thresholds::default(), now, utc_offset: 8 * 3600,
        });
        assert!(!ok[0].pace.is_empty());
        let mut ok = ok;
        ok[1].pace = "5 小时额度用得较快，预计 星期三 00:25 用完".into(); // about the longest line
        assert_eq!(size(1.0, &ok), size(1.0, &panel), "the panel keeps its size whether or not a pace is shown");
        for (name, panel) in [("panel", &panel), ("free", &free), ("ok", &ok)] {
            for light in [false, true] {
                for scale in [1.0, 1.75] {
                    let (w, h) = size(scale, panel);
                    let mut surface = Surface::new(w, h).unwrap();
                    let mut scratch = Surface::new(w, h).unwrap();
                    paint(&mut surface, &mut scratch, panel, light, scale);
                    assert!(surface.pixels().iter().all(|p| p >> 24 == 255));
                    if let Some(dir) = std::env::var_os("IUSAGE_TEST_RENDER_DIR") {
                        let dir = std::path::PathBuf::from(dir);
                        std::fs::create_dir_all(&dir).unwrap();
                        let mut bmp = Vec::new();
                        bmp.extend_from_slice(b"BM");
                        bmp.extend_from_slice(&(54 + w as u32 * h as u32 * 4).to_le_bytes());
                        bmp.extend_from_slice(&[0; 4]);
                        bmp.extend_from_slice(&54u32.to_le_bytes());
                        bmp.extend_from_slice(&40u32.to_le_bytes());
                        bmp.extend_from_slice(&w.to_le_bytes());
                        bmp.extend_from_slice(&(-h).to_le_bytes());
                        bmp.extend_from_slice(&1u16.to_le_bytes());
                        bmp.extend_from_slice(&32u16.to_le_bytes());
                        bmp.extend_from_slice(&[0; 24]);
                        for pixel in surface.pixels() { bmp.extend_from_slice(&pixel.to_le_bytes()); }
                        let name = format!("{name}-{}-{}.bmp", if light { "light" } else { "dark" }, (scale * 100.0) as u32);
                        std::fs::write(dir.join(name), bmp).unwrap();
                    }
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Face {
    Title,
    Body,
    BodyBold,
    Small,
    Pace,
}

struct Text<'a> {
    face: Face,
    text: &'a str,
    /// Top and height of the line, in layout units.
    y: f32,
    h: f32,
    align: Align,
    color: Color,
}

pub fn paint(surface: &mut Surface, scratch: &mut Surface, panel: &Panel, light: bool, scale: f32) {
    let s = scale;
    let (background, primary, secondary, track, rule) = if light {
        (0xFFF3_F3F3u32, Color::rgba(0, 0, 0, 228), Color::rgba(0, 0, 0, 150), Color::rgba(0, 0, 0, 36), Color::rgba(0, 0, 0, 30))
    } else {
        (0xFF2C_2C2Cu32, Color::rgba(255, 255, 255, 236), Color::rgba(255, 255, 255, 150), Color::rgba(255, 255, 255, 40), Color::rgba(255, 255, 255, 28))
    };
    surface.pixels().fill(background);

    // Shapes are drawn while walking the layout; text is collected and drawn afterwards,
    // because both need the surface.
    let mut texts: Vec<Text> = Vec::new();
    let inner = WIDTH - PAD * 2.0;
    let mut y = TOP;
    {
        let mut canvas = surface.canvas();
        for (i, section) in panel.iter().enumerate() {
            if i > 0 {
                y += SECTION_GAP;
                canvas.round_rect(PAD * s, (y * s).round(), inner * s, s.round().max(1.0), 0.0, f32::MAX, rule);
                y += 1.0 + SECTION_GAP;
            }
            let (_, accent) = badge(section.provider);
            canvas.circle((PAD + 4.0) * s, (y + HEADER_H / 2.0) * s, 4.0 * s, accent);
            texts.push(Text { face: Face::Title, text: section.provider.name(), y, h: HEADER_H, align: Align::Left, color: primary });
            if section.current {
                texts.push(Text { face: Face::Small, text: "任务栏显示中", y, h: HEADER_H, align: Align::Right, color: secondary });
            }
            y += HEADER_H + HEADER_GAP;

            if section.unsubscribed {
                texts.push(Text { face: Face::Small, text: &section.status, y, h: STATUS_H, align: Align::Left, color: secondary });
                y += STATUS_H;
                let (text, color) = link(section).map_or((section.retry.as_str(), secondary), |link| (link, LINK_TEXT));
                texts.push(Text { face: Face::Small, text, y, h: STATUS_H, align: Align::Left, color });
                y += STATUS_H;
                continue;
            }

            for (n, row) in section.rows.iter().enumerate() {
                if n > 0 {
                    y += ROW_GAP;
                }
                texts.push(Text { face: Face::Body, text: row.name, y, h: LINE_H, align: Align::Left, color: primary });
                texts.push(Text { face: Face::BodyBold, text: &row.value, y, h: LINE_H, align: Align::Right, color: primary });
                y += LINE_H + BAR_GAP;
                canvas.round_rect(PAD * s, y * s, inner * s, BAR_H * s, BAR_H * s / 2.0, f32::MAX, track);
                if let Some(fill) = row.fill.filter(|f| *f > 0.0) {
                    let right = (PAD + inner * fill) * s;
                    canvas.round_rect(PAD * s, y * s, inner * s, BAR_H * s, BAR_H * s / 2.0, right, fill_color(row.severity));
                }
                y += BAR_H + DETAIL_GAP;
                texts.push(Text { face: Face::Small, text: &row.detail, y, h: DETAIL_H, align: Align::Left, color: secondary });
                y += DETAIL_H;
            }

            y += STATUS_GAP;
            let color = if section.failed { WARNING_TEXT } else { secondary };
            texts.push(Text { face: Face::Small, text: &section.status, y, h: STATUS_H, align: Align::Left, color });
            y += STATUS_H;
            texts.push(Text { face: Face::Small, text: &section.provenance, y, h: STATUS_H, align: Align::Left, color: secondary });
            y += STATUS_H;
            // The pace stands apart from the status lines, larger than the body text.
            y += PACE_GAP;
            let color = if section.pace_warn { WARNING_TEXT } else { primary };
            texts.push(Text { face: Face::Pace, text: &section.pace, y, h: PACE_H, align: Align::Left, color });
            y += PACE_H;
            texts.push(Text { face: Face::Small, text: &section.retry, y, h: STATUS_H, align: Align::Left, color: secondary });
            y += STATUS_H;
            if let Some(link) = link(section) {
                texts.push(Text { face: Face::Small, text: link, y, h: STATUS_H, align: Align::Left, color: LINK_TEXT });
            }
            y += STATUS_H;
        }
    }

    let px = |size: f32| (size * s).round() as i32;
    let title = Font::new(px(14.0), true);
    let body = Font::new(px(12.0), false);
    let body_bold = Font::new(px(12.0), true);
    let small = Font::new(px(11.0), false);
    let pace = Font::new(px(13.0), false);
    for t in texts {
        let font = match t.face {
            Face::Title => &title,
            Face::Body => &body,
            Face::BodyBold => &body_bold,
            Face::Small => &small,
            Face::Pace => &pace,
        };
        // The title sits to the right of the provider dot.
        let left = if matches!(t.face, Face::Title) { PAD + 14.0 } else { PAD };
        let rect = RECT {
            left: (left * s).round() as i32,
            top: (t.y * s).round() as i32,
            right: ((WIDTH - PAD) * s).round() as i32,
            bottom: ((t.y + t.h) * s).round() as i32,
        };
        draw_text(surface, scratch, font, t.text, rect, t.align, t.color);
    }
}

use super::{Report, dot};
use anyhow::{Context, Result};
use fontdue::{Font, FontSettings};
use image::{Rgb, RgbImage};

const WHITE: Rgb<u8> = Rgb([244, 247, 250]);
const GREEN: Rgb<u8> = Rgb([32, 230, 110]);
const AMBER: Rgb<u8> = Rgb([255, 184, 40]);
const CYAN: Rgb<u8> = Rgb([40, 205, 255]);

fn font() -> Result<Font> {
    let bytes = std::fs::read("/System/Library/Fonts/STHeiti Medium.ttc")
        .context("无法读取水印中文字体 STHeiti Medium")?;
    Font::from_bytes(bytes, FontSettings::default())
        .map_err(|e| anyhow::anyhow!("无法加载水印字体：{e}"))
}

fn text(img: &mut RgbImage, font: &Font, value: &str, x: u32, y: u32, px: f32, color: Rgb<u8>) {
    let mut cursor = x as f32;
    for ch in value.chars() {
        let (m, bitmap) = font.rasterize(ch, px);
        for row in 0..m.height {
            for col in 0..m.width {
                let dx = cursor.round() as i32 + m.xmin + col as i32;
                let dy = y as i32 + px.ceil() as i32 - m.ymin - m.height as i32 + row as i32;
                if dx < 0 || dy < 0 || dx >= img.width() as i32 || dy >= img.height() as i32 {
                    continue;
                }
                let alpha = bitmap[row * m.width + col] as u32;
                let old = img.get_pixel_mut(dx as u32, dy as u32);
                for c in 0..3 {
                    old[c] =
                        ((old[c] as u32 * (255 - alpha) + color[c] as u32 * alpha) / 255) as u8;
                }
            }
        }
        cursor += m.advance_width;
    }
}

fn wrap(font: &Font, value: &str, px: f32, width: u32) -> Vec<String> {
    let mut rows = vec![];
    let mut row = String::new();
    let mut used = 0.;
    for ch in value.chars() {
        let advance = font.metrics(ch, px).advance_width;
        if ch == '\n' || (used + advance > width as f32 && !row.is_empty()) {
            rows.push(std::mem::take(&mut row));
            used = 0.;
        }
        if ch != '\n' {
            row.push(ch);
            used += advance;
        }
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows
}

fn shade(img: &mut RgbImage, x: u32, y: u32, w: u32, h: u32) {
    for py in y..(y + h).min(img.height()) {
        for px in x..(x + w).min(img.width()) {
            let p = img.get_pixel_mut(px, py);
            for c in 0..3 {
                p[c] = (p[c] as u32 * 55 / 255 + 12) as u8;
            }
        }
    }
}

fn explanation(report: &Report) -> String {
    if report.measurement_method == "ears"
        && !matches!(report.assessment, "unknown" | "away")
        && let Some(ratio) = report.head_clearance_ratio
    {
        let mut lines = vec![
            format!("头肩比 {ratio:.2}；低于 0.42 提醒"),
            "0.42–0.46 观察；≥0.46 未触发低头".into(),
            "头肩比＝双耳到肩线高度 / 肩宽".into(),
        ];
        if let Some(tilt) = report.head_tilt_relative_shoulders_deg {
            lines.push(format!("相对歪头 {:.1}°；达到18°提醒", tilt.abs()));
        }
        return lines.join("\n");
    }
    // Use the actual fallback/unknown explanation, never substitute ear-based thresholds.
    report
        .note
        .split('。')
        .take(2)
        .collect::<Vec<_>>()
        .join("。\n")
}

pub(super) fn paint(img: &mut RgbImage, report: &Report) -> Result<()> {
    let font = font()?;
    let px = (img.width() as f32 / 48.).clamp(12., 22.);
    let line_h = (px * 1.5).ceil() as u32;
    let margin = 12u32.min(img.width() / 20);
    let panel_w = (img.width() / 2).min(460);
    let content_w = panel_w.saturating_sub(24).max(1);
    // Place the legend on the opposite side of the detected person.
    let x = if report.roi[0] + report.roi[2] / 2 < img.width() / 2 {
        img.width().saturating_sub(panel_w + margin)
    } else {
        margin
    };
    let state = match report.assessment {
        "needs_reminder" => "需要提醒：疑似低头或歪头",
        "no_obvious_issue" => "本次未触发提醒",
        "observe" => "临界待观察，暂不提醒",
        "away" => "离座或非坐姿，本次不评",
        _ => "无法判断，关键点不足或不适用",
    };
    let state_color = match report.assessment {
        "needs_reminder" => Rgb([255, 100, 100]),
        "no_obvious_issue" => GREEN,
        _ => AMBER,
    };
    let sections = [
        (format!("李老师 · {state}"), state_color),
        (explanation(report), WHITE),
        ("绿点/绿线：较高分关节及连接".into(), GREEN),
        ("橙点：较低分候选，仅供参考".into(), AMBER),
        ("青线：头顶、颈部及双肩辅助线".into(), CYAN),
        ("红线（如有）：表示已触发提醒".into(), Rgb([255, 100, 100])),
        ("青框：本次人体检测区域".into(), CYAN),
        (
            "二维估计，遮挡点可能猜错；未提醒不等于坐姿合格。".into(),
            WHITE,
        ),
    ];
    let rows: Vec<_> = sections
        .iter()
        .flat_map(|(s, color)| {
            wrap(&font, s, px, content_w)
                .into_iter()
                .map(move |s| (s, *color))
        })
        .collect();
    let y = 34.min(img.height() / 10);
    shade(img, x, y, panel_w, rows.len() as u32 * line_h + 20);
    for (i, (s, color)) in rows.iter().enumerate() {
        text(img, &font, s, x + 12, y + 8 + i as u32 * line_h, px, *color);
    }
    // Name only landmarks used by the rules, keeping the rest of the skeleton uncluttered.
    for i in [3, 4, 5, 6, 17, 18] {
        let Some(p) = report.keypoints.get(i) else {
            continue;
        };
        if !p.in_roi || p.score < 0.35 {
            continue;
        }
        let label_px = (px * 0.8).max(12.);
        let width = p.name.chars().count() as u32 * label_px.ceil() as u32 + 8;
        let lx = (p.x.max(0.) as u32 + 8).min(img.width().saturating_sub(width));
        let ly = (p.y.max(0.) as u32).saturating_sub(label_px as u32 + 4);
        shade(img, lx, ly, width, label_px.ceil() as u32 + 8);
        text(
            img,
            &font,
            p.name,
            lx + 4,
            ly + 1,
            label_px,
            if p.score >= 0.5 { GREEN } else { AMBER },
        );
        dot(
            img,
            p.x.round() as i32,
            p.y.round() as i32,
            3,
            if p.score >= 0.5 { GREEN } else { AMBER },
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chinese_watermark_wraps_to_panel_width_and_renders_visible_glyphs() -> Result<()> {
        let font = font()?;
        let message = "本次未触发提醒，遮挡点可能猜错";
        let rows = wrap(&font, message, 18., 120);
        assert_eq!(rows.concat(), message);
        assert!(rows.len() > 1);
        for row in rows {
            let width: f32 = row
                .chars()
                .map(|c| font.metrics(c, 18.).advance_width)
                .sum();
            assert!(width <= 120.);
        }
        assert!(font.has_glyph('肩'));
        let mut image = RgbImage::from_pixel(200, 40, Rgb([0, 0, 0]));
        text(&mut image, &font, "头顶与双肩", 4, 4, 18., GREEN);
        assert!(image.pixels().filter(|p| p[1] > 50).count() > 100);
        Ok(())
    }
}

//! Top-view mouse diagram for the Buttons tab, painted with plain egui shapes (no image assets). Each tag is a
//! click target that selects the matching table row; the selected tag is inverted.

use super::theme::*;
use super::widgets::*;
use eframe::egui::{self, pos2, vec2, Align2, Color32, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Ui, epaint::RectShape};

/// What the diagram needs to know about one button row.
pub struct Tag {
    pub id: u8,
    pub name: &'static str,
    pub current: String,
    pub is_default: bool,
    pub dirty: bool,
}

pub const HEIGHT: f32 = 316.0;

/// Returns the button id of a tag clicked this frame.
pub fn mouse_panel(ui: &mut Ui, p: &Pal, tags_in: &[Tag], selected: Option<u8>) -> Option<u8> {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, HEIGHT), Sense::hover());
    let painter = ui.painter().clone();
    painter.add(Shape::Rect(RectShape::new(rect, R0, p.raised, Stroke::new(1.0, p.i18()), StrokeKind::Inside)));
    let c = pos2(rect.center().x, rect.top() + 150.0);
    let hw = |y: f32| -> f32 {
        // superellipse silhouette: narrower at the nose, fullest behind the wheel
        let u = (y / 112.0).clamp(-1.0, 1.0);
        let base = 52.0 + 10.0 * (u * 0.5 + 0.5);
        base * (1.0 - u.abs().powf(2.7)).max(0.0).powf(1.0 / 2.7)
    };
    let mut left_pts = Vec::new();
    let mut right_pts = Vec::new();
    let n = 64;
    for i in 0..=n {
        let y = -112.0 + 224.0 * (i as f32 / n as f32);
        left_pts.push(pos2(c.x - hw(y), c.y + y));
        right_pts.push(pos2(c.x + hw(y), c.y + y));
    }
    let mut body = left_pts;
    right_pts.reverse();
    body.extend(right_pts);
    let mut outline = egui::epaint::PathShape::closed_line(body, Stroke::new(1.5, p.i45()));
    outline.fill = p.i06();
    painter.add(Shape::Path(outline));
    // button seams
    let seam = Stroke::new(1.0, p.i28());
    let y_seam = -14.0;
    painter.line_segment([pos2(c.x, c.y - 112.0), pos2(c.x, c.y + y_seam)], seam);
    painter.line_segment([pos2(c.x - hw(y_seam), c.y + y_seam), pos2(c.x + hw(y_seam), c.y + y_seam)], seam);
    // wheel + scroll arrows
    let wheel = Rect::from_center_size(pos2(c.x, c.y - 48.0), vec2(14.0, 36.0));
    painter.add(Shape::Rect(RectShape::new(wheel, R0, p.i10(), Stroke::new(1.0, p.i65()), StrokeKind::Inside)));
    painter.text(pos2(c.x, c.y - 76.0), Align2::CENTER_CENTER, "▲", mono(8.0), p.i65());
    painter.text(pos2(c.x, c.y - 22.0), Align2::CENTER_CENTER, "▼", mono(8.0), p.i65());
    // side buttons (left edge)
    let side = |y0: f32, y1: f32| {
        let x = c.x - hw((y0 + y1) / 2.0);
        Rect::from_min_max(pos2(x - 7.0, c.y + y0), pos2(x + 1.0, c.y + y1))
    };
    let fwd = side(-56.0, -28.0);
    let back = side(-8.0, 20.0);
    for r in [fwd, back] {
        painter.add(Shape::Rect(RectShape::new(r, R0, p.i10(), Stroke::new(1.0, p.i65()), StrokeKind::Inside)));
    }
    // DPI: underside, drawn dashed because it cannot be seen from above
    let dpi_c = pos2(c.x, c.y + 54.0);
    painter.add(Shape::dashed_line(&circle_pts(dpi_c, 15.0), Stroke::new(1.0, p.i65()), 3.0, 3.0));
    painter.text(dpi_c, Align2::CENTER_CENTER, "DPI", mono(T_MICRO - 1.0), p.i65());
    ptext(ui, pos2(c.x, c.y + 86.0), Align2::CENTER_CENTER, "UNDERSIDE", mono(T_MICRO - 1.0), p.i45(), 0.12);

    // tags: (button id, left column?, tag-centre y offset, anchor on the mouse). Anchors are ordered top-to-bottom
    // like their tags so leader lines never cross.
    let tags: [(u8, bool, f32, Pos2); 8] = [
        (1, true, -90.0, pos2(c.x - 30.0, c.y - 90.0)),
        (5, true, -42.0, pos2(fwd.left(), fwd.center().y)),
        (4, true, 6.0, pos2(back.left(), back.center().y)),
        (0x60, true, 54.0, pos2(dpi_c.x - 15.0, dpi_c.y)),
        (2, false, -90.0, pos2(c.x + 30.0, c.y - 90.0)),
        (9, false, -42.0, pos2(c.x + 4.0, c.y - 76.0)),
        (3, false, 6.0, pos2(wheel.right(), wheel.center().y)),
        (10, false, 54.0, pos2(c.x + 4.0, c.y - 22.0)),
    ];
    let tag_w = ((w - 120.0) / 2.0 - 4.0).clamp(96.0, 128.0);
    let tag_h = 40.0;
    let mut clicked = None;
    for (id, left, dy, anchor) in tags {
        let Some(t) = tags_in.iter().find(|t| t.id == id) else { continue };
        let is_sel = selected == Some(id);
        let cy = c.y + dy;
        let x = if left { rect.left() + 8.0 } else { rect.right() - 8.0 - tag_w };
        let tag = Rect::from_min_size(pos2(x, cy - tag_h / 2.0), vec2(tag_w, tag_h));
        let resp = ui.interact(tag, ui.id().with(("mouse-tag", id)), Sense::click());
        resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, is_sel, format!("{}: {}", t.name, t.current)));
        if resp.clicked() {
            clicked = Some(id);
        }
        let hot = resp.hovered() && !is_sel;
        let edge = if left { pos2(tag.right(), tag.center().y) } else { pos2(tag.left(), tag.center().y) };
        let (fill, stroke, fg1, fg2) = if is_sel {
            (p.stamp_bg, p.stamp_bg, p.stamp_fg, p.stamp_fg)
        } else if hot {
            (p.i10(), p.i100(), p.i100(), p.i100())
        } else {
            (Color32::TRANSPARENT, p.i28(), p.i100(), if t.is_default { p.i65() } else { p.i100() })
        };
        painter.line_segment([edge, anchor], Stroke::new(1.0, if is_sel || hot { p.i100() } else { p.i28() }));
        painter.circle_filled(anchor, 2.5, if is_sel || hot { p.i100() } else { p.i65() });
        painter.add(Shape::Rect(RectShape::new(tag, R0, fill, Stroke::new(1.0, stroke), StrokeKind::Inside)));
        ptext(ui, pos2(tag.left() + 8.0, tag.top() + 7.0), Align2::LEFT_TOP, &t.name.to_uppercase(), mono_med(T_MICRO), fg1, 0.12);
        ptext(ui, pos2(tag.left() + 8.0, tag.bottom() - 7.0), Align2::LEFT_BOTTOM, &t.current, mono(T_MICRO + 0.5), fg2, 0.0);
        if t.dirty {
            painter.text(pos2(tag.right() - 8.0, tag.top() + 6.0), Align2::RIGHT_TOP, "◐", mono(T_MICRO), fg1);
        }
        if t.id == 1 {
            painter.text(pos2(tag.right() - 8.0, tag.top() + 6.0), Align2::RIGHT_TOP, "○", mono(T_MICRO), fg1);
        }
    }
    ptext(ui, pos2(rect.center().x, rect.bottom() - 14.0), Align2::CENTER_CENTER, "TOP VIEW", mono(T_MICRO - 1.0), p.i45(), 0.16);
    clicked
}

fn circle_pts(c: Pos2, r: f32) -> Vec<Pos2> {
    (0..=48).map(|i| {
        let a = i as f32 / 48.0 * std::f32::consts::TAU;
        pos2(c.x + r * a.cos(), c.y + r * a.sin())
    }).collect()
}

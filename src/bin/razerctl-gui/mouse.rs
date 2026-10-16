//! Mouse diagram for the Buttons tab: a hairline top-view wireframe of the Razer Viper Mini Signature Edition,
//! painted with plain egui shapes from the mm-coordinate lists below (no image assets). Each tag is a click target
//! that selects the matching table row; the selected tag is inverted and its physical feature is inked.
//! Geometry and look are the signed-off spec: DESIGN.md "Mouse wireframe (DAN-1531)" (DAN-1532).

use super::theme::*;
use super::widgets::*;
use eframe::egui::{
    self,
    epaint::{PathShape, RectShape},
    pos2, vec2, Align2, Color32, CornerRadius, Painter, Pos2, Rect, Sense, Shape, Stroke,
    StrokeKind, Ui,
};

/// What the diagram needs to know about one button row.
pub struct Tag {
    pub id: u8,
    pub name: &'static str,
    pub current: String,
    pub is_default: bool,
    pub dirty: bool,
}

pub const HEIGHT: f32 = 316.0;

/// Traced geometry, millimetres, origin at the body centre, +x right, +y toward the tail. Left halves only; `mirror_*`
/// rebuilds the right. Kept verbatim from the spec, so rustfmt leaves the lists alone.
#[rustfmt::skip]
mod data {
    #[cfg(test)]
    pub const LEN_MM: f32 = 118.6;
    pub const WID_MM: f32 = 61.6;

    // ---- traced data (left half; points run nose -> tail or as listed) ---------------------------------------------
    pub const OUTLINE_HALF: &[(f32, f32)] = &[(0.0, -59.3), (-7.3, -59.1), (-14.3, -58.1), (-20.9, -55.7), (-25.6, -51.9), (-28.2, -46.7), (-29.6, -39.5), (-30.3, -30.0), (-30.6, -18.1), (-30.3, -6.2), (-29.9, 3.3), (-30.1, 14.1), (-30.8, 24.8), (-30.8, 31.9), (-30.1, 37.2), (-28.5, 41.0), (-26.1, 44.8), (-23.5, 48.6), (-20.7, 51.7), (-17.4, 54.5), (-13.9, 56.7), (-9.4, 58.3), (-4.9, 59.1), (0.0, 59.3)];
    pub const SHOULDER_HALF: &[(f32, f32)] = &[(0.0, -57.2), (-7.3, -56.9), (-13.9, -56.0), (-19.5, -53.8), (-23.7, -50.3), (-26.1, -45.5), (-27.3, -39.1), (-28.0, -30.0), (-28.2, -18.1), (-27.3, -6.2), (-26.3, 3.3), (-27.0, 14.1), (-28.0, 24.8), (-28.5, 31.9), (-28.0, 36.7), (-26.6, 40.5), (-24.5, 43.8), (-22.1, 47.2), (-19.5, 50.0), (-16.2, 52.6), (-12.7, 54.5), (-8.5, 56.0), (-4.2, 56.7), (0.0, 56.9)];
    pub const CHEVRON_HALF: &[(f32, f32)] = &[(0.0, -9.3), (-4.9, -9.3), (-6.3, -8.3), (-12.5, 1.0), (-21.9, 7.1)];
    pub const WIN_A: &[(f32, f32)] = &[(-26.6, -19.5), (-12.0, -12.4), (-21.4, -9.1), (-24.7, -7.6), (-26.1, -12.9)];
    pub const WIN_B: &[(f32, f32)] = &[(-10.1, -8.6), (-23.7, -4.0), (-22.6, 2.9), (-12.9, 1.2)];
    pub const WIN_C1_HALF: &[(f32, f32)] = &[(0.0, -2.1), (-3.3, -2.1), (-5.9, 2.9), (-4.5, 4.0), (0.0, 4.0)];
    pub const WIN_C2_HALF: &[(f32, f32)] = &[(0.0, 7.9), (-8.5, 7.9), (0.0, 21.4)];
    pub const WIN_D: &[(f32, f32)] = &[(-19.0, 12.4), (-15.3, 10.5), (-17.2, 15.5), (-18.1, 18.1), (-18.8, 17.9)];
    pub const WIN_E: &[(f32, f32)] = &[(-11.1, 12.6), (-16.0, 21.9), (-4.9, 22.1)];
    pub const WIN_F: &[(f32, f32)] = &[(-16.9, 26.7), (-4.7, 26.9), (-11.1, 39.3)];
    pub const WIN_G_HALF: &[(f32, f32)] = &[(0.0, 29.1), (-7.3, 42.2), (0.0, 42.2)];
    pub const WIN_H: &[(f32, f32)] = &[(-19.5, 28.8), (-24.0, 41.0), (-13.9, 41.9), (-16.7, 34.8)];
    pub const WIN_J: &[(f32, f32)] = &[(-23.5, 44.3), (-11.1, 45.0), (-9.9, 53.6), (-14.3, 51.9), (-20.9, 48.1)];
    pub const WIN_K_HALF: &[(f32, f32)] = &[(0.0, 45.0), (-7.3, 45.0), (-6.3, 54.5), (0.0, 54.8)];

    // ---- hand-set features (mm) --------------------------------------------------------------------------------------
    /// Front/rear channel flanking the wheel: x = +-WELL_HX, from the nose lip to the rear end of the slot.
    pub const WELL_HX: f32 = 4.9;
    pub const CHAN_FRONT: (f32, f32) = (-56.2, -47.2);
    pub const WELL_Y: (f32, f32) = (-47.2, -27.9);
    pub const CHAN_REAR: (f32, f32) = (-27.9, -20.7);
    pub const WHEEL_HX: f32 = 3.5;
    pub const WHEEL_Y: (f32, f32) = (-45.0, -29.3);
    /// Thin L/R seam between the rear channel and the plate-edge chevron.
    pub const SEAM_Y: (f32, f32) = (-18.7, -9.3);
    /// Side buttons on the left edge: y range, x range (they straddle the outline by 0.6 mm).
    pub const SIDE_FWD_Y: (f32, f32) = (-20.5, -6.0);
    pub const SIDE_BACK_Y: (f32, f32) = (-4.0, 13.5);
    pub const SIDE_X: (f32, f32) = (-31.4, -27.6);
    /// DPI button (underside; position is a guess, see DESIGN.md).
    pub const DPI_C: (f32, f32) = (0.0, 12.6);
    pub const DPI_R: f32 = 4.4;
    /// Plate contour ribs, left plate: (start y at the channel edge, bezier control, end) — mirrored for the right plate.
    pub const RIBS: &[[(f32, f32); 3]] = &[
        [(-5.4, -52.0), (-16.0, -53.6), (-25.6, -47.4)],
        [(-5.4, -42.0), (-17.0, -44.0), (-27.2, -36.4)],
        [(-5.4, -32.0), (-17.0, -34.0), (-27.8, -25.6)],
    ];

}
use data::*;

/// mm -> screen. `c` is the body centre, `k` is px per mm.
#[derive(Clone, Copy)]
struct Xf {
    c: Pos2,
    k: f32,
}
impl Xf {
    fn pt(&self, x: f32, y: f32) -> Pos2 {
        pos2(self.c.x + x * self.k, self.c.y + y * self.k)
    }
    fn pts(&self, v: &[(f32, f32)]) -> Vec<Pos2> {
        v.iter().map(|&(x, y)| self.pt(x, y)).collect()
    }
}

/// Left-half open path (axis point first, axis point last) -> closed outline.
fn mirror_closed(half: &[(f32, f32)]) -> Vec<(f32, f32)> {
    let mut v = half.to_vec();
    for &(x, y) in half.iter().rev().skip(1).take(half.len().saturating_sub(2)) {
        v.push((-x, y));
    }
    v
}

/// Chaikin corner cutting on a closed polygon: rounds corners, keeps the shape within ~r of the input. `cut` is the
/// fraction of each edge trimmed at both ends (0.25 = classic Chaikin; smaller keeps corners crisper).
fn chaikin_cut(p: &[Pos2], iters: u32, cut: f32) -> Vec<Pos2> {
    let mut cur = p.to_vec();
    for _ in 0..iters {
        let n = cur.len();
        let mut next = Vec::with_capacity(n * 2);
        for i in 0..n {
            let (a, b) = (cur[i], cur[(i + 1) % n]);
            next.push(a + (b - a) * cut);
            next.push(a + (b - a) * (1.0 - cut));
        }
        cur = next;
    }
    cur
}

fn quad(a: Pos2, c: Pos2, b: Pos2) -> Vec<Pos2> {
    (0..=16)
        .map(|i| {
            let t = i as f32 / 16.0;
            let u = 1.0 - t;
            pos2(
                u * u * a.x + 2.0 * u * t * c.x + t * t * b.x,
                u * u * a.y + 2.0 * u * t * c.y + t * t * b.y,
            )
        })
        .collect()
}

fn circle_pts(c: Pos2, r: f32) -> Vec<Pos2> {
    (0..=48)
        .map(|i| {
            let a = i as f32 / 48.0 * std::f32::consts::TAU;
            pos2(c.x + r * a.cos(), c.y + r * a.sin())
        })
        .collect()
}

/// The smoothed silhouette in mm (what `paint_body` strokes, before the mm -> px scale).
#[cfg(test)]
fn outline_mm() -> Vec<Pos2> {
    let unit = Xf {
        c: Pos2::ZERO,
        k: 1.0,
    };
    chaikin_cut(&unit.pts(&mirror_closed(OUTLINE_HALF)), 2, 0.25)
}

fn side_rect(xf: Xf, y: (f32, f32)) -> Rect {
    Rect::from_min_max(xf.pt(SIDE_X.0, y.0), xf.pt(SIDE_X.1, y.1))
}

/// Screen anchors for the eight tags' leader lines.
struct Anchors {
    lmb: Pos2,
    rmb: Pos2,
    wheel_r: Pos2,
    up: Pos2,
    down: Pos2,
    fwd: Pos2,
    back: Pos2,
    dpi_l: Pos2,
}

/// Where each tag's leader line lands (pure geometry, so it can be unit-tested without a painter).
fn anchors(xf: Xf) -> Anchors {
    let (fwd, back) = (side_rect(xf, SIDE_FWD_Y), side_rect(xf, SIDE_BACK_Y));
    let dpi = xf.pt(DPI_C.0, DPI_C.1);
    Anchors {
        lmb: xf.pt(-14.0, -54.0),
        rmb: xf.pt(14.0, -54.0),
        wheel_r: xf.pt(WHEEL_HX, (WHEEL_Y.0 + WHEEL_Y.1) / 2.0),
        up: xf.pt(4.2, -51.6),
        down: xf.pt(4.2, -24.3),
        fwd: pos2(fwd.left(), fwd.center().y),
        back: pos2(back.left(), back.center().y),
        dpi_l: pos2(dpi.x - DPI_R * xf.k, dpi.y),
    }
}

/// Tag column width: 104 px is enough for the longest stock label ("dpi-cycle-up") and leaves the mouse the middle.
fn tag_w(w: f32) -> f32 {
    ((w - 120.0) / 2.0 - 4.0).clamp(96.0, 104.0)
}

/// Px-per-mm for a panel `w` px wide: the mouse fills the gap between the two tag columns.
fn scale_for(w: f32, tag_w: f32) -> f32 {
    ((w - 2.0 * (8.0 + tag_w) - 8.0) / WID_MM).clamp(1.45, 1.9)
}

/// (button id, left column?, tag-centre y offset from `c`, anchor on the mouse). Anchors are ordered top-to-bottom like
/// their tags in each column, so leader lines never cross (checked by the `leaders_do_not_cross` test).
fn tag_slots(a: &Anchors) -> [(u8, bool, f32, Pos2); 8] {
    [
        (1, true, -90.0, a.lmb),
        (5, true, -42.0, a.fwd),
        (4, true, 6.0, a.back),
        (0x60, true, 54.0, a.dpi_l),
        (2, false, -90.0, a.rmb),
        (9, false, -42.0, a.up),
        (3, false, 6.0, a.wheel_r),
        (10, false, 54.0, a.down),
    ]
}

/// Paint the wireframe. `hot` holds the button ids of the selected and hovered tags: their physical features are
/// inked at 100 % so the eye can pair a tag with its part (the plates take an ink-18 wash).
fn paint_body(painter: &Painter, p: &Pal, xf: Xf, hot: &[u8]) {
    let is = |id: u8| hot.contains(&id);
    let pick = |on: bool| Stroke::new(1.0, if on { p.i100() } else { p.i65() });
    let wash = |on: bool| if on { p.i18() } else { p.i10() };
    let ink28 = Stroke::new(1.0, p.i28());
    let ink18 = Stroke::new(1.0, p.i18());
    let ink45 = Stroke::new(1.0, p.i45());

    // 1. outline (silhouette), the only filled shape
    let outline = chaikin_cut(&xf.pts(&mirror_closed(OUTLINE_HALF)), 2, 0.25);
    let mut o = PathShape::closed_line(outline, Stroke::new(1.5, p.i45()));
    o.fill = p.i06();
    painter.add(Shape::Path(o));
    // 2. shoulder line (where the top surface meets the side wall)
    let sh = chaikin_cut(&xf.pts(&mirror_closed(SHOULDER_HALF)), 2, 0.25);
    painter.add(Shape::Path(PathShape::closed_line(sh, ink18)));
    // 3. plate contour ribs
    for r in RIBS {
        for s in [-1.0_f32, 1.0] {
            let q = |(x, y): (f32, f32)| xf.pt(x * -s, y);
            painter.add(Shape::line(quad(q(r[0]), q(r[1]), q(r[2])), ink18));
        }
    }
    // 3b. a selected or hovered plate (left = 1, right = 2) takes a wash; the windows below punch holes in it
    for (id, s) in [(1u8, -1.0_f32), (2, 1.0)] {
        if is(id) {
            let mut v: Vec<(f32, f32)> = vec![(-WELL_HX, -56.8)];
            v.extend(SHOULDER_HALF.iter().skip(1).filter(|q| q.1 <= 3.3).cloned());
            v.extend([(-21.9, 7.1), (-12.5, 1.0), (-6.3, -8.3), (-WELL_HX, -9.3)]);
            let pts: Vec<Pos2> = v.iter().map(|&(x, y)| xf.pt(x * -s, y)).collect();
            let mut sh = PathShape::closed_line(pts, Stroke::NONE);
            sh.fill = p.i18();
            painter.add(Shape::Path(sh));
        }
    }
    // 4. lattice windows: holes in the shell, filled with the panel colour so they read as cut-outs
    let mut wins: Vec<Vec<(f32, f32)>> = Vec::new();
    for w in [WIN_A, WIN_B, WIN_D, WIN_E, WIN_F, WIN_H, WIN_J] {
        wins.push(w.to_vec());
        wins.push(w.iter().map(|&(x, y)| (-x, y)).rev().collect());
    }
    for h in [WIN_C1_HALF, WIN_C2_HALF, WIN_G_HALF, WIN_K_HALF] {
        wins.push(mirror_closed(h));
    }
    for w in wins {
        let pts = chaikin_cut(&xf.pts(&w), 1, 0.2);
        let mut s = PathShape::closed_line(pts, ink28);
        s.fill = p.raised;
        painter.add(Shape::Path(s));
    }
    // 5. seams: plate-edge chevron + centre seam
    for s in [-1.0_f32, 1.0] {
        let v: Vec<Pos2> = CHEVRON_HALF
            .iter()
            .map(|&(x, y)| xf.pt(x * -s, y))
            .collect();
        painter.add(Shape::line(v, ink28));
    }
    painter.line_segment([xf.pt(0.0, SEAM_Y.0), xf.pt(0.0, SEAM_Y.1)], ink28);
    // 6. wheel channel + well + wheel
    for s in [-1.0_f32, 1.0] {
        let x = s * WELL_HX;
        painter.line_segment([xf.pt(x, CHAN_FRONT.0), xf.pt(x, CHAN_FRONT.1)], ink28);
        painter.line_segment([xf.pt(x, CHAN_REAR.0), xf.pt(x, CHAN_REAR.1 - 1.6)], ink28);
    }
    let well = Rect::from_min_max(xf.pt(-WELL_HX, WELL_Y.0), xf.pt(WELL_HX, WELL_Y.1));
    painter.add(Shape::Rect(RectShape::new(
        well,
        CornerRadius::same((1.6 * xf.k) as u8),
        p.raised,
        ink45,
        StrokeKind::Inside,
    )));
    let wheel = Rect::from_min_max(xf.pt(-WHEEL_HX, WHEEL_Y.0), xf.pt(WHEEL_HX, WHEEL_Y.1));
    painter.add(Shape::Rect(RectShape::new(
        wheel,
        CornerRadius::same((0.9 * xf.k) as u8),
        wash(is(3)),
        pick(is(3)),
        StrokeKind::Inside,
    )));
    let mut y = WHEEL_Y.0 + 1.6;
    while y < WHEEL_Y.1 - 1.0 {
        painter.line_segment([xf.pt(-WHEEL_HX + 0.3, y), xf.pt(WHEEL_HX - 0.3, y)], ink45);
        y += 1.6;
    }
    // 7. scroll arrows live in the channels above and below the wheel
    let arrow = |on: bool| if on { p.i100() } else { p.i65() };
    painter.text(
        xf.pt(0.0, -51.6),
        Align2::CENTER_CENTER,
        "▲",
        mono(8.0),
        arrow(is(9)),
    );
    painter.text(
        xf.pt(0.0, -24.3),
        Align2::CENTER_CENTER,
        "▼",
        mono(8.0),
        arrow(is(10)),
    );
    // 8. side buttons (left edge)
    for (r, id) in [
        (side_rect(xf, SIDE_FWD_Y), 5u8),
        (side_rect(xf, SIDE_BACK_Y), 4u8),
    ] {
        painter.add(Shape::Rect(RectShape::new(
            r,
            CornerRadius::same(2),
            wash(is(id)),
            pick(is(id)),
            StrokeKind::Inside,
        )));
    }
    // 9. DPI: underside, drawn dashed because it cannot be seen from above; sits in the big central window with no
    // label inside (the tag and the legend row name it)
    let dpi = xf.pt(DPI_C.0, DPI_C.1);
    painter.add(Shape::dashed_line(
        &circle_pts(dpi, DPI_R * xf.k),
        pick(is(0x60)),
        3.0,
        3.0,
    ));
}

/// Returns the button id of a tag clicked this frame.
pub fn mouse_panel(ui: &mut Ui, p: &Pal, tags_in: &[Tag], selected: Option<u8>) -> Option<u8> {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, HEIGHT), Sense::hover());
    let painter = ui.painter().clone();
    painter.add(Shape::Rect(RectShape::new(
        rect,
        R0,
        p.raised,
        Stroke::new(1.0, p.i18()),
        StrokeKind::Inside,
    )));
    let tag_w = tag_w(w);
    let xf = Xf {
        c: pos2(rect.center().x, rect.top() + 150.0),
        k: scale_for(w, tag_w),
    };
    let c = xf.c;
    let a = anchors(xf);
    let tag_h = 40.0;
    // interact first: whether a tag is hovered decides which part of the mouse is inked, and the body is painted
    // before the leaders and tags that sit on top of it
    let mut clicked = None;
    let mut slots = Vec::new();
    for (id, left, dy, anchor) in tag_slots(&a) {
        let Some(t) = tags_in.iter().find(|t| t.id == id) else {
            continue;
        };
        let x = if left {
            rect.left() + 8.0
        } else {
            rect.right() - 8.0 - tag_w
        };
        let tag = Rect::from_min_size(pos2(x, c.y + dy - tag_h / 2.0), vec2(tag_w, tag_h));
        let resp = ui.interact(tag, ui.id().with(("mouse-tag", id)), Sense::click());
        let is_sel = selected == Some(id);
        resp.widget_info(|| {
            egui::WidgetInfo::selected(
                egui::WidgetType::Button,
                true,
                is_sel,
                format!("{}: {}", t.name, t.current),
            )
        });
        if resp.clicked() {
            clicked = Some(id);
        }
        slots.push((t, left, tag, anchor, is_sel, resp.hovered() && !is_sel));
    }
    let hot_ids: Vec<u8> = slots
        .iter()
        .filter(|s| s.4 || s.5)
        .map(|s| s.0.id)
        .collect();
    paint_body(&painter, p, xf, &hot_ids);
    for (t, left, tag, anchor, is_sel, hot) in slots {
        let edge = if left {
            pos2(tag.right(), tag.center().y)
        } else {
            pos2(tag.left(), tag.center().y)
        };
        let (fill, stroke, fg1, fg2) = if is_sel {
            (p.stamp_bg, p.stamp_bg, p.stamp_fg, p.stamp_fg)
        } else if hot {
            (p.i10(), p.i100(), p.i100(), p.i100())
        } else {
            (
                Color32::TRANSPARENT,
                p.i28(),
                p.i100(),
                if t.is_default { p.i65() } else { p.i100() },
            )
        };
        painter.line_segment(
            [edge, anchor],
            Stroke::new(1.0, if is_sel || hot { p.i100() } else { p.i28() }),
        );
        painter.circle_filled(anchor, 2.5, if is_sel || hot { p.i100() } else { p.i65() });
        painter.add(Shape::Rect(RectShape::new(
            tag,
            R0,
            fill,
            Stroke::new(1.0, stroke),
            StrokeKind::Inside,
        )));
        ptext(
            ui,
            pos2(tag.left() + 8.0, tag.top() + 7.0),
            Align2::LEFT_TOP,
            &t.name.to_uppercase(),
            mono_med(T_MICRO),
            fg1,
            0.12,
        );
        ptext(
            ui,
            pos2(tag.left() + 8.0, tag.bottom() - 7.0),
            Align2::LEFT_BOTTOM,
            &t.current,
            mono(T_MICRO + 0.5),
            fg2,
            0.0,
        );
        if t.dirty {
            painter.text(
                pos2(tag.right() - 8.0, tag.top() + 6.0),
                Align2::RIGHT_TOP,
                "◐",
                mono(T_MICRO),
                fg1,
            );
        }
        if t.id == 1 {
            painter.text(
                pos2(tag.right() - 8.0, tag.top() + 6.0),
                Align2::RIGHT_TOP,
                "○",
                mono(T_MICRO),
                fg1,
            );
        }
    }
    // legend row: the view name, and what the dashed stroke means
    let ly = rect.bottom() - 14.0;
    ptext(
        ui,
        pos2(rect.center().x - 6.0, ly),
        Align2::RIGHT_CENTER,
        "TOP VIEW",
        mono(T_MICRO - 1.0),
        p.i45(),
        0.16,
    );
    let sx = rect.center().x + 6.0;
    painter.add(Shape::dashed_line(
        &[pos2(sx, ly), pos2(sx + 18.0, ly)],
        Stroke::new(1.0, p.i65()),
        3.0,
        3.0,
    ));
    ptext(
        ui,
        pos2(sx + 24.0, ly),
        Align2::LEFT_CENTER,
        "UNDERSIDE",
        mono(T_MICRO - 1.0),
        p.i45(),
        0.12,
    );
    clicked
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bbox(pts: &[Pos2]) -> Rect {
        Rect::from_points(pts)
    }

    /// Even-odd point-in-polygon.
    fn inside(poly: &[Pos2], q: Pos2) -> bool {
        let mut c = false;
        for i in 0..poly.len() {
            let (a, b) = (poly[i], poly[(i + 1) % poly.len()]);
            if (a.y > q.y) != (b.y > q.y) && q.x < (b.x - a.x) * (q.y - a.y) / (b.y - a.y) + a.x {
                c = !c;
            }
        }
        c
    }

    /// The silhouette keeps the published Viper Mini SE proportions (118.6 x 61.6 mm), not a generic mouse's.
    #[test]
    fn outline_aspect_matches_the_published_size() {
        let b = bbox(&outline_mm());
        let (w, l) = (b.width(), b.height());
        assert!((l - LEN_MM).abs() < 0.8, "length {l} mm vs {LEN_MM}");
        assert!((w - WID_MM).abs() < 0.8, "width {w} mm vs {WID_MM}");
        let (got, want) = (l / w, LEN_MM / WID_MM);
        assert!(
            (got - want).abs() < 0.04 * want,
            "L:W {got:.3} vs {want:.3}"
        );
    }

    /// Widest half-extent of the outline among points with `y` in `lo..=hi` mm.
    fn half_width(outline: &[Pos2], lo: f32, hi: f32) -> f32 {
        outline
            .iter()
            .filter(|q| q.y >= lo && q.y <= hi)
            .map(|q| q.x.abs())
            .fold(0.0, f32::max)
    }

    /// The Viper Mini SE has a waist behind the side buttons: the body is narrower there than at the front shoulders
    /// and at the rear. The old generic superellipse only ever widened toward the tail.
    #[test]
    fn outline_has_the_waist() {
        let o = outline_mm();
        let (front, waist, rear) = (
            half_width(&o, -24.0, -12.0),
            half_width(&o, 0.0, 8.0),
            half_width(&o, 20.0, 34.0),
        );
        assert!(waist + 0.5 < front, "waist {waist} mm vs front {front}");
        assert!(waist + 0.5 < rear, "waist {waist} mm vs rear {rear}");
    }

    /// Every leader lands on the mouse: inside the outline, or on a side button (which straddles the edge by design).
    #[test]
    fn tag_anchors_lie_on_or_inside_the_outline() {
        for w in [250.0_f32, 306.0, 360.0] {
            let xf = Xf {
                c: pos2(w / 2.0, 150.0),
                k: scale_for(w, tag_w(w)),
            };
            let outline = chaikin_cut(&xf.pts(&mirror_closed(OUTLINE_HALF)), 2, 0.25);
            let a = anchors(xf);
            let sides = [
                side_rect(xf, SIDE_FWD_Y).expand(0.5),
                side_rect(xf, SIDE_BACK_Y).expand(0.5),
            ];
            for (name, q) in [
                ("lmb", a.lmb),
                ("rmb", a.rmb),
                ("wheel_r", a.wheel_r),
                ("up", a.up),
                ("down", a.down),
                ("fwd", a.fwd),
                ("back", a.back),
                ("dpi_l", a.dpi_l),
            ] {
                assert!(
                    inside(&outline, q) || sides.iter().any(|r| r.contains(q)),
                    "w={w}: {name} anchor {q:?} is off the mouse"
                );
            }
        }
    }

    fn ccw(a: Pos2, b: Pos2, c: Pos2) -> f32 {
        (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
    }
    fn cross(a: (Pos2, Pos2), b: (Pos2, Pos2)) -> bool {
        let (d1, d2) = (ccw(b.0, b.1, a.0), ccw(b.0, b.1, a.1));
        let (d3, d4) = (ccw(a.0, a.1, b.0), ccw(a.0, a.1, b.1));
        d1 * d2 < 0.0 && d3 * d4 < 0.0
    }

    /// Leader lines (tag edge -> anchor) must not cross, at every panel width the layout can produce.
    #[test]
    fn leaders_do_not_cross() {
        for w in [250.0_f32, 306.0, 360.0] {
            let tag_w = tag_w(w);
            let c = pos2(w / 2.0, 150.0);
            let a = anchors(Xf {
                c,
                k: scale_for(w, tag_w),
            });
            let mut segs = Vec::new();
            for (_, left, dy, anchor) in tag_slots(&a) {
                let edge_x = if left { 8.0 + tag_w } else { w - 8.0 - tag_w };
                segs.push((pos2(edge_x, c.y + dy), anchor));
            }
            for i in 0..segs.len() {
                for j in i + 1..segs.len() {
                    assert!(!cross(segs[i], segs[j]), "w={w}: leader {i} crosses {j}");
                }
            }
        }
    }
}

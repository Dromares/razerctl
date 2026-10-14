//! Reusable pieces of the redesign, built from stock egui (Frame / painter / Sense). Every interactive widget
//! here draws its own hover, pressed, focus and disabled states, because egui's `Button::fill()` would pin one
//! colour for every state.

use super::theme::*;
use eframe::egui::{
    self, epaint::RectShape, text::LayoutJob, vec2, Align, Align2, Color32, CornerRadius, FontId, Frame, Layout, Margin,
    Pos2, Rect, Response, RichText, Sense, Shape, Stroke, StrokeKind, TextFormat, Ui, WidgetInfo, WidgetType,
};
use std::sync::Arc;

pub const R0: CornerRadius = CornerRadius::ZERO;

/// Mono caps label with the design's tracking (egui supports `extra_letter_spacing`).
pub fn tracked(text: &str, font: FontId, color: Color32, track_em: f32) -> RichText {
    let sp = track_em * font.size;
    RichText::new(text).font(font).color(color).extra_letter_spacing(sp)
}

pub fn caps(p: &Pal, text: &str) -> RichText {
    tracked(&text.to_uppercase(), mono(T_LABEL), p.i65(), TRACK_LABEL)
}

fn galley(ui: &Ui, text: &str, font: FontId, color: Color32, track_em: f32) -> Arc<egui::Galley> {
    let mut job = LayoutJob::default();
    job.append(text, 0.0, TextFormat { font_id: font.clone(), color, extra_letter_spacing: track_em * font.size, ..Default::default() });
    ui.painter().layout_job(job)
}

/// Paint tracked text at a point (mouse diagram, switch: no layout flow there).
pub fn ptext(ui: &Ui, pos: Pos2, anchor: Align2, text: &str, font: FontId, color: Color32, track_em: f32) -> Rect {
    let g = galley(ui, text, font, color, track_em);
    let rect = anchor.anchor_size(pos, g.size());
    ui.painter().galley(rect.min, g, color);
    rect
}

/// "01 // STATUS ─────────────" — numbered section label with a trailing hairline.
pub fn section(ui: &mut Ui, p: &Pal, num: &str, title: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = S3;
        ui.label(caps(p, &format!("{num} // {title}")));
        let y = ui.cursor().center().y;
        let x0 = ui.cursor().left();
        let x1 = ui.max_rect().right();
        ui.painter().hline(x0..=x1, y, Stroke::new(1.0, p.i18()));
        ui.allocate_space(vec2((x1 - x0).max(0.0), 16.0));
    });
}

/// Raised panel: 1px hairline border, no radius, no shadow.
pub fn panel<R>(ui: &mut Ui, p: &Pal, margin: i8, add: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::new()
        .fill(p.raised)
        .stroke(Stroke::new(1.0, p.i18()))
        .inner_margin(Margin::same(margin))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

pub fn hair(ui: &mut Ui, p: &Pal) {
    let r = ui.available_rect_before_wrap();
    ui.painter().hline(r.x_range(), ui.cursor().top(), Stroke::new(1.0, p.i18()));
    ui.add_space(1.0);
}

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Primary,
    Secondary,
}

#[derive(Clone, Copy, PartialEq)]
enum St {
    Rest,
    Hover,
    Pressed,
    Disabled,
}

fn state(r: &Response, enabled: bool) -> St {
    if !enabled { St::Disabled } else if r.is_pointer_button_down_on() { St::Pressed } else if r.hovered() || r.has_focus() { St::Hover } else { St::Rest }
}

fn focus_ring(ui: &Ui, p: &Pal, r: &Response, rect: Rect) {
    if r.has_focus() {
        ui.painter().rect_stroke(rect.expand(2.0), R0, Stroke::new(1.0, p.i100()), StrokeKind::Outside);
    }
}

/// Button. Primary = inversion (stamp), Secondary = hairline outline. Disabled keeps its shape, loses its ink.
pub fn btn(ui: &mut Ui, p: &Pal, label: &str, kind: Kind, enabled: bool) -> Response {
    btn_w(ui, p, label, kind, enabled, 0.0)
}

pub fn btn_w(ui: &mut Ui, p: &Pal, label: &str, kind: Kind, enabled: bool, min_w: f32) -> Response {
    let text = label.to_uppercase();
    let g = galley(ui, &text, mono_med(T_LABEL), Color32::PLACEHOLDER, 0.12);
    let size = vec2((g.size().x + 2.0 * S4).max(min_w), 32.0);
    let (rect, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, label));
    let (fill, stroke, fg) = match (kind, state(&resp, enabled)) {
        (Kind::Primary, St::Rest) => (p.stamp_bg, p.stamp_bg, p.stamp_fg),
        (Kind::Primary, St::Hover) => (p.over_stamp(0.14), p.i100(), p.stamp_fg),
        (Kind::Primary, St::Pressed) => (p.sunken, p.i100(), p.i100()),
        (Kind::Primary, St::Disabled) => (p.i06(), p.i18(), p.i45()),
        (Kind::Secondary, St::Rest) => (Color32::TRANSPARENT, p.i45(), p.i100()),
        (Kind::Secondary, St::Hover) => (p.i10(), p.i100(), p.i100()),
        (Kind::Secondary, St::Pressed) => (p.stamp_bg, p.stamp_bg, p.stamp_fg),
        (Kind::Secondary, St::Disabled) => (Color32::TRANSPARENT, p.i18(), p.i45()),
    };
    ui.painter().rect(rect, R0, fill, Stroke::new(1.0, stroke), StrokeKind::Inside);
    ui.painter().galley_with_override_text_color(rect.center() - g.size() / 2.0, g, fg);
    focus_ring(ui, p, &resp, rect);
    resp
}

#[derive(Clone, Copy, PartialEq)]
pub enum ChipStyle {
    Plain,
    Outline,
    Stamp,
}

/// Small mono status chip: optional glyph + caps text. Glyphs: ● good/on  ○ none  ◐ pending  × error  ▲ attention.
/// Status is never colour alone: the glyph and the word carry it, colour only reinforces.
pub fn chip(ui: &mut Ui, p: &Pal, glyph: Option<&str>, text: &str, style: ChipStyle, color: Color32) -> Response {
    let (fill, stroke, fg) = match style {
        ChipStyle::Plain => (Color32::TRANSPARENT, Stroke::NONE, color),
        ChipStyle::Outline => (Color32::TRANSPARENT, Stroke::new(1.0, p.i28()), color),
        ChipStyle::Stamp => (p.stamp_bg, Stroke::new(1.0, p.stamp_bg), p.stamp_fg),
    };
    let margin = if style == ChipStyle::Plain { Margin::symmetric(0, 2) } else { Margin::symmetric(8, 3) };
    Frame::new().fill(fill).stroke(stroke).inner_margin(margin).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            if let Some(g) = glyph {
                ui.label(RichText::new(g).font(mono(T_LABEL)).color(fg));
            }
            ui.label(tracked(&text.to_uppercase(), mono_med(T_LABEL), fg, 0.10));
        });
    }).response
}

/// Hairline segmented meter (battery). `frac` 0..1.
pub fn meter(ui: &mut Ui, p: &Pal, width: f32, height: f32, frac: f32, segments: u32) {
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    ui.painter().rect_stroke(rect, R0, Stroke::new(1.0, p.i28()), StrokeKind::Inside);
    let inner = rect.shrink(2.0);
    let gap = 2.0;
    let seg_w = (inner.width() - gap * (segments as f32 - 1.0)) / segments as f32;
    let lit = (frac * segments as f32).round() as u32;
    for i in 0..segments {
        let x = inner.left() + i as f32 * (seg_w + gap);
        let r = Rect::from_min_size(Pos2::new(x, inner.top()), vec2(seg_w, inner.height()));
        let c = if i < lit { p.i100() } else { p.i10() };
        ui.painter().add(Shape::Rect(RectShape::filled(r, R0, c)));
    }
}

/// Segmented control (tabs, polling rate). One bordered box; the active segment is inverted. Returns the index
/// clicked this frame, if any. `active` may be `None` (no segment matches).
pub fn segmented(ui: &mut Ui, p: &Pal, id: &str, labels: &[String], active: Option<usize>, min_w: f32) -> Option<usize> {
    let gs: Vec<_> = labels.iter().map(|l| galley(ui, &l.to_uppercase(), mono_med(T_LABEL), Color32::PLACEHOLDER, TRACK_LABEL)).collect();
    let widths: Vec<f32> = gs.iter().map(|g| (g.size().x + 20.0).max(min_w)).collect();
    let total: f32 = widths.iter().sum();
    let (outer, _) = ui.allocate_exact_size(vec2(total, 34.0), Sense::hover());
    let mut clicked = None;
    let mut x = outer.left();
    for (i, g) in gs.into_iter().enumerate() {
        let seg = Rect::from_min_size(Pos2::new(x, outer.top()), vec2(widths[i], outer.height()));
        x += widths[i];
        let resp = ui.interact(seg, ui.id().with((id, i)), Sense::click());
        resp.widget_info(|| WidgetInfo::selected(WidgetType::Button, true, active == Some(i), labels[i].clone()));
        let on = active == Some(i);
        let (fill, fg) = match (on, state(&resp, true)) {
            (true, St::Pressed) => (p.over_stamp(0.35), p.stamp_fg),
            (true, _) => (p.stamp_bg, p.stamp_fg),
            (false, St::Pressed) => (p.i28(), p.i100()),
            (false, St::Hover) => (p.i10(), p.i100()),
            (false, _) => (Color32::TRANSPARENT, p.i65()),
        };
        ui.painter().rect_filled(seg, R0, fill);
        if i > 0 {
            ui.painter().vline(seg.left(), seg.y_range(), Stroke::new(1.0, p.i18()));
        }
        ui.painter().galley_with_override_text_color(seg.center() - g.size() / 2.0, g, fg);
        focus_ring(ui, p, &resp, seg.shrink(2.0));
        if resp.clicked() {
            clicked = Some(i);
        }
    }
    ui.painter().rect_stroke(outer, R0, Stroke::new(1.0, p.i45()), StrokeKind::Inside);
    clicked
}

/// Squared OFF/ON switch plus its label; the whole row is the click target (as the old checkbox's label was).
/// Marks the response `changed` when toggled.
pub fn switch(ui: &mut Ui, p: &Pal, on: &mut bool, label: &str) -> Response {
    let g = galley(ui, label, sans(T_BODY - 1.0), p.i100(), 0.0);
    let sw = vec2(72.0, 28.0);
    let (rect, mut resp) = ui.allocate_exact_size(vec2(sw.x + S3 + g.size().x, sw.y), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    resp.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, true, *on, label));
    let st = state(&resp, true);
    let sw_rect = Rect::from_min_size(rect.min, sw);
    let edge = match st { St::Hover | St::Pressed => p.i100(), _ => p.i45() };
    ui.painter().rect_stroke(sw_rect, R0, Stroke::new(1.0, edge), StrokeKind::Inside);
    let half = Rect::from_min_size(
        if *on { Pos2::new(sw_rect.center().x, sw_rect.top()) } else { sw_rect.min },
        vec2(sw_rect.width() / 2.0, sw_rect.height()),
    );
    ui.painter().add(Shape::Rect(RectShape::filled(half.shrink(2.0), R0, if *on { p.stamp_bg } else { p.i10() })));
    let (c_off, c_on) = if *on { (p.i65(), p.stamp_fg) } else { (p.i100(), p.i45()) };
    ptext(ui, Pos2::new(sw_rect.left() + sw_rect.width() * 0.25, sw_rect.center().y), Align2::CENTER_CENTER, "OFF", mono_med(T_MICRO), c_off, 0.12);
    ptext(ui, Pos2::new(sw_rect.left() + sw_rect.width() * 0.75, sw_rect.center().y), Align2::CENTER_CENTER, "ON", mono_med(T_MICRO), c_on, 0.12);
    ui.painter().galley(Pos2::new(sw_rect.right() + S3, rect.center().y - g.size().y / 2.0), g, p.i100());
    focus_ring(ui, p, &resp, sw_rect);
    resp
}

/// Key/value row for the Device status panel: mono caps label left, value in a shared column.
pub fn kv(ui: &mut Ui, p: &Pal, key: &str, label_w: f32, value: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.allocate_ui_with_layout(vec2(label_w, 22.0), Layout::left_to_right(Align::Center), |ui| {
            ui.set_min_width(label_w);
            ui.label(caps(p, key));
        });
        ui.allocate_ui_with_layout(vec2(ui.available_width(), 22.0), Layout::left_to_right(Align::Center), value);
    });
}

impl Pal {
    /// The stamp fill nudged toward its foreground: hover / pressed feedback for inverted controls.
    pub fn over_stamp(&self, t: f32) -> Color32 {
        let m = |a: u8, b: u8| (a as f32 * (1.0 - t) + b as f32 * t).round() as u8;
        Color32::from_rgb(m(self.stamp_bg.r(), self.stamp_fg.r()), m(self.stamp_bg.g(), self.stamp_fg.g()), m(self.stamp_bg.b(), self.stamp_fg.b()))
    }
}

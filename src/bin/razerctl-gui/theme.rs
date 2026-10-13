//! "Single-ink dossier" tokens, fonts and global egui style for razerctl-gui.
//!
//! One ink on charcoal (dark) or warm paper (light), an alpha ladder for hierarchy, hairline frames, no shadows,
//! no radius. Signal colours (ok / unsaved / failed) are always on and only ever used on `bg` / `raised`
//! surfaces: the light "ok" green fails contrast on `sunken` (4.35:1).

use eframe::egui::{self, Color32, FontData, FontDefinitions, FontFamily, FontId};
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Dark,
    Light,
}

#[derive(Clone, Copy)]
pub struct Pal {
    pub mode: Mode,
    pub bg: Color32,     // page
    pub raised: Color32, // panels, table, strip
    pub sunken: Color32, // text fields, editor, button tester
    pub ink: Color32,
    ink_rgb: [u8; 3],
    a65: f32,
    a45: f32,
    a28: f32,
    a18: f32,
    a10: f32,
    a06: f32,
    a05: f32,
    /// Inversion: primary button, active tab / segment, newest key press, error status bar.
    pub stamp_bg: Color32,
    pub stamp_fg: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub bad: Color32,
}

fn rgb(h: u32) -> Color32 {
    Color32::from_rgb((h >> 16) as u8, (h >> 8) as u8, h as u8)
}

fn ink_a(c: [u8; 3], a: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(c[0], c[1], c[2], (a * 255.0).round() as u8)
}

impl Pal {
    pub fn new(mode: Mode) -> Self {
        let (bg, raised, sunken, ink, a, stamp_fg, sig) = match mode {
            Mode::Dark => (
                rgb(0x0d0d0d), rgb(0x141414), rgb(0x090909), [0xcd, 0xc4, 0xba],
                [0.65, 0.45, 0.28, 0.18, 0.10, 0.06, 0.05], rgb(0x0d0d0d),
                [rgb(0x8fc79a), rgb(0xe0b252), rgb(0xf08a78)],
            ),
            Mode::Light => (
                rgb(0xe8e1d2), rgb(0xf2ede1), rgb(0xddd2bd), [0x21, 0x1d, 0x17],
                [0.70, 0.50, 0.45, 0.32, 0.12, 0.07, 0.08], rgb(0xf6f1e6),
                [rgb(0x1f6b3a), rgb(0x7a4f00), rgb(0xa32a1a)],
            ),
        };
        let inkc = Color32::from_rgb(ink[0], ink[1], ink[2]);
        Pal {
            mode, bg, raised, sunken, ink: inkc, ink_rgb: ink,
            a65: a[0], a45: a[1], a28: a[2], a18: a[3], a10: a[4], a06: a[5], a05: a[6],
            stamp_bg: inkc, stamp_fg, ok: sig[0], warn: sig[1], bad: sig[2],
        }
    }
    pub fn i100(&self) -> Color32 { self.ink }
    pub fn i65(&self) -> Color32 { ink_a(self.ink_rgb, self.a65) }
    pub fn i45(&self) -> Color32 { ink_a(self.ink_rgb, self.a45) }
    pub fn i28(&self) -> Color32 { ink_a(self.ink_rgb, self.a28) }
    pub fn i18(&self) -> Color32 { ink_a(self.ink_rgb, self.a18) }
    pub fn i10(&self) -> Color32 { ink_a(self.ink_rgb, self.a10) }
    pub fn i06(&self) -> Color32 { ink_a(self.ink_rgb, self.a06) }
    pub fn i05(&self) -> Color32 { ink_a(self.ink_rgb, self.a05) }

    /// Alpha token composited over an opaque surface (contrast measurement in the tests).
    #[cfg(test)]
    pub fn over(&self, a: f32, surface: Color32) -> Color32 {
        let m = |f: u8, s: u8| (f as f32 * a + s as f32 * (1.0 - a)).round() as u8;
        Color32::from_rgb(m(self.ink_rgb[0], surface.r()), m(self.ink_rgb[1], surface.g()), m(self.ink_rgb[2], surface.b()))
    }
}

// ---- type scale (px). Display / title: serif. Body: grotesk. Labels / data: mono. ----
pub const T_DISPLAY: f32 = 36.0; // tab title "Buttons."
pub const T_NUMERAL: f32 = 30.0; // battery %, DPI value
pub const T_TITLE: f32 = 22.0; // empty-state / service state
pub const T_BODY: f32 = 15.0;
pub const T_SMALL: f32 = 13.0;
pub const T_DATA: f32 = 12.0;
pub const T_LABEL: f32 = 11.0; // mono CAPS labels
pub const T_MICRO: f32 = 10.0;
pub const TRACK_LABEL: f32 = 0.16; // em, applied via extra_letter_spacing

// ---- spacing scale ----
pub const S1: f32 = 4.0;
pub const S2: f32 = 8.0;
pub const S3: f32 = 12.0;
pub const S4: f32 = 16.0;
pub const S5: f32 = 24.0;
pub const S6: f32 = 32.0;

pub fn serif(sz: f32) -> FontId { FontId::new(sz, FontFamily::Name("serif".into())) }
pub fn sans(sz: f32) -> FontId { FontId::new(sz, FontFamily::Proportional) }
pub fn mono(sz: f32) -> FontId { FontId::new(sz, FontFamily::Monospace) }
pub fn mono_med(sz: f32) -> FontId { FontId::new(sz, FontFamily::Name("mono-medium".into())) }
pub fn mono_bold(sz: f32) -> FontId { FontId::new(sz, FontFamily::Name("mono-bold".into())) }
pub fn kanji(sz: f32) -> FontId { FontId::new(sz, FontFamily::Name("kanji".into())) }

macro_rules! font {
    ($defs:expr, $name:expr, $file:expr) => {
        $defs.font_data.insert(
            $name.to_string(),
            Arc::new(FontData::from_static(include_bytes!(concat!("../../../assets/fonts/", $file)))),
        );
    };
}

/// Registers the bundled faces (all SIL OFL, see assets/fonts/OFL.txt). egui loads TTF/OTF only.
pub fn install_fonts(ctx: &egui::Context) {
    let mut d = FontDefinitions::default();
    font!(d, "grotesk", "SpaceGrotesk-Regular.ttf");
    font!(d, "jb", "JetBrainsMono-Regular.ttf");
    font!(d, "jb-med", "JetBrainsMono-Medium.ttf");
    font!(d, "jb-bold", "JetBrainsMono-Bold.ttf");
    font!(d, "serif", "SourceSerif4-Regular.ttf");
    font!(d, "kanji", "RyokuKanji.ttf");
    font!(d, "status", "RyokuStatus.ttf");

    let defaults_prop = d.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let defaults_mono = d.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
    let with_fallback = |first: &[&str], fb: &[String]| -> Vec<String> {
        first.iter().map(|s| s.to_string()).chain(fb.iter().cloned()).collect()
    };
    // Status glyphs (● ○ ◐ × · ▲) are not all in JetBrains Mono: the Ryoku Status subset goes right behind it.
    // egui's default fonts stay as the tail of every family, so emoji (🐍 💾 🔋) and anything else still resolve.
    d.families.insert(FontFamily::Proportional, with_fallback(&["grotesk", "status"], &defaults_prop));
    d.families.insert(FontFamily::Monospace, with_fallback(&["jb", "status"], &defaults_mono));
    d.families.insert(FontFamily::Name("mono-medium".into()), with_fallback(&["jb-med", "status"], &defaults_mono));
    d.families.insert(FontFamily::Name("mono-bold".into()), with_fallback(&["jb-bold", "status"], &defaults_mono));
    d.families.insert(FontFamily::Name("serif".into()), with_fallback(&["serif", "status"], &defaults_prop));
    d.families.insert(FontFamily::Name("kanji".into()), vec!["kanji".into()]);
    ctx.set_fonts(d);
}

/// Global egui style: every stock widget (ComboBox, TextEdit, DragValue, Slider, popups) inherits this.
pub fn apply_style(ctx: &egui::Context, p: &Pal) {
    use egui::{CornerRadius, Stroke};
    ctx.set_visuals(if p.mode == Mode::Dark { egui::Visuals::dark() } else { egui::Visuals::light() });
    ctx.all_styles_mut(|s| {
        let v = &mut s.visuals;
        v.panel_fill = p.bg;
        v.window_fill = p.raised;
        v.extreme_bg_color = p.sunken; // TextEdit / code editor background
        v.faint_bg_color = p.i06();
        v.override_text_color = None;
        v.window_stroke = Stroke::new(1.0, p.i45());
        v.window_corner_radius = CornerRadius::ZERO;
        v.menu_corner_radius = CornerRadius::ZERO;
        v.popup_shadow = egui::epaint::Shadow::NONE;
        v.window_shadow = egui::epaint::Shadow::NONE;
        v.selection.bg_fill = p.i28();
        v.selection.stroke = Stroke::new(1.0, p.i100());
        v.text_cursor.stroke = Stroke::new(1.5, p.i100());
        let r0 = CornerRadius::ZERO;
        let w = &mut v.widgets;
        w.noninteractive.bg_fill = p.raised;
        w.noninteractive.weak_bg_fill = p.raised;
        w.noninteractive.bg_stroke = Stroke::new(1.0, p.i18());
        w.noninteractive.fg_stroke = Stroke::new(1.0, p.i65());
        w.noninteractive.corner_radius = r0;
        w.inactive.bg_fill = p.sunken;
        w.inactive.weak_bg_fill = Color32::TRANSPARENT;
        // control boundary: ink-45 = 3.09:1 dark / 3.14:1 light on raised (WCAG 1.4.11 non-text 3:1)
        w.inactive.bg_stroke = Stroke::new(1.0, p.i45());
        w.inactive.fg_stroke = Stroke::new(1.0, p.i100());
        w.inactive.corner_radius = r0;
        w.hovered.bg_fill = p.i10();
        w.hovered.weak_bg_fill = p.i10();
        w.hovered.bg_stroke = Stroke::new(1.0, p.i100());
        w.hovered.fg_stroke = Stroke::new(1.0, p.i100());
        w.hovered.corner_radius = r0;
        w.active.bg_fill = p.stamp_bg;
        w.active.weak_bg_fill = p.stamp_bg;
        w.active.bg_stroke = Stroke::new(1.0, p.i100());
        w.active.fg_stroke = Stroke::new(1.0, p.stamp_fg);
        w.active.corner_radius = r0;
        w.open.bg_fill = p.sunken;
        w.open.weak_bg_fill = p.sunken;
        w.open.bg_stroke = Stroke::new(1.0, p.i100());
        w.open.fg_stroke = Stroke::new(1.0, p.i100());
        w.open.corner_radius = r0;
        v.handle_shape = egui::style::HandleShape::Rect { aspect_ratio: 0.45 };
        v.slider_trailing_fill = true;
        s.spacing.item_spacing = egui::vec2(S2, S2);
        s.spacing.button_padding = egui::vec2(S3, S2);
        s.spacing.interact_size.y = 32.0;
        s.spacing.combo_width = 0.0;
        s.spacing.slider_rail_height = 3.0;
        s.spacing.slider_width = 220.0;
        s.spacing.scroll.bar_width = 6.0;
    });
}

// ---------- WCAG contrast (used by the unit tests that pin the design's contrast claims) ----------

#[cfg(test)]
pub fn contrast(a: Color32, b: Color32) -> f64 {
    fn lin(c: u8) -> f64 {
        let c = c as f64 / 255.0;
        if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    }
    fn lum(c: Color32) -> f64 { 0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b()) }
    let (x, y) = (lum(a), lum(b));
    let (hi, lo) = if x > y { (x, y) } else { (y, x) };
    (hi + 0.05) / (lo + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODES: [Mode; 2] = [Mode::Dark, Mode::Light];

    #[test]
    fn text_tokens_clear_wcag_aa_on_their_surfaces() {
        for m in MODES {
            let p = Pal::new(m);
            let a65 = if m == Mode::Dark { 0.65 } else { 0.70 };
            for s in [p.bg, p.raised, p.sunken] {
                assert!(contrast(p.ink, s) >= 4.5, "{m:?} ink on {s:?}");
                assert!(contrast(p.over(a65, s), s) >= 4.5, "{m:?} ink-65 on {s:?}");
            }
            assert!(contrast(p.stamp_fg, p.stamp_bg) >= 4.5, "{m:?} stamp");
        }
    }

    #[test]
    fn control_boundary_clears_non_text_3_to_1_on_raised() {
        for m in MODES {
            let p = Pal::new(m);
            let a = if m == Mode::Dark { 0.45 } else { 0.50 };
            assert!(contrast(p.over(a, p.raised), p.raised) >= 3.0, "{m:?}");
        }
    }

    #[test]
    fn signal_colours_clear_4_5_on_raised_and_bg() {
        for m in MODES {
            let p = Pal::new(m);
            for s in [p.raised, p.bg] {
                for (n, c) in [("ok", p.ok), ("warn", p.warn), ("bad", p.bad)] {
                    assert!(contrast(c, s) >= 4.5, "{m:?} {n} on {s:?}: {:.2}", contrast(c, s));
                }
            }
        }
    }
}

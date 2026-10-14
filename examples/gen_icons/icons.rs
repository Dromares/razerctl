//! The five symbolic tray icons (`packaging/icons/razerctl-*-symbolic.svg`) as strings.
//!
//! Same 22-unit geometry as the DAN-1452 mockup (`tray-icons.png`) and as `pixmap()` in src/bin/razerctl-gui/tray.rs.
//! Everything is a filled shape (no strokes): GTK recolours symbolic icons by `fill` only, KDE by the
//! ColorScheme-Text class. The badge's knock-out gap is a clip path, the glyph cut into the badge is a hole.
//!
//! This file is shared by `examples/gen_icons/main.rs` (writes the files) and `tests/icons.rs` (asserts the committed
//! files are byte-identical to what this produces). It is dev-time only: nothing under `src/` includes it.
//! The numeric formatting reproduces the Python this was ported from (DAN-1480): `{:g}` is `%g` (6 significant
//! digits, trailing zeros stripped) and `{:.2f}` is `%.2f`.

use std::fmt::Write;

const STYLE: &str =
    r#"<style type="text/css" id="current-color-scheme">.ColorScheme-Text{color:#232629}</style>"#;

/// Python's `format(x, "g")`: 6 significant digits, fixed notation for exponents -4..=5, trailing zeros stripped.
pub fn g(x: f64) -> String {
    if x == 0.0 {
        return if x.is_sign_negative() { "-0" } else { "0" }.into();
    }
    // `{:.5e}` rounds exactly (and bumps the exponent on 9.999995 -> 1.00000e1), so the exponent it reports is the one `%g` uses.
    let sci = format!("{x:.5e}");
    let (mant, exp) = sci
        .split_once('e')
        .expect("scientific format has an exponent");
    let exp: i32 = exp.parse().expect("integer exponent");
    if (-4..6).contains(&exp) {
        let fixed = format!("{x:.*}", (5 - exp) as usize);
        strip_zeros(&fixed).to_string()
    } else {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{}e{sign}{:02}", strip_zeros(mant), exp.abs())
    }
}

fn strip_zeros(s: &str) -> &str {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.')
    } else {
        s
    }
}

/// Rounded-rect outline, clockwise from the top edge.
fn rrect(x0: f64, y0: f64, x1: f64, y1: f64, r: f64) -> String {
    let (w, h) = (x1 - x0, y1 - y0);
    format!(
        "M{} {}h{}a{r} {r} 0 0 1 {r} {r}v{}a{r} {r} 0 0 1 {nr} {r}h{}a{r} {r} 0 0 1 {nr} {nr}v{}a{r} {r} 0 0 1 {r} {nr}Z",
        g(x0 + r),
        g(y0),
        g(w - 2.0 * r),
        g(h - 2.0 * r),
        g(-(w - 2.0 * r)),
        g(-(h - 2.0 * r)),
        r = g(r),
        nr = g(-r),
    )
}

fn circle(cx: f64, cy: f64, r: f64) -> String {
    format!(
        "M{} {}a{r} {r} 0 1 0 {d} 0a{r} {r} 0 1 0 {nd} 0Z",
        g(cx - r),
        g(cy),
        r = g(r),
        d = g(2.0 * r),
        nd = g(-2.0 * r),
    )
}

const WHEEL: [&str; 3] = [
    "M6.8 8.7h8.4v1.6H6.8Z",   // chin line
    "M10.2 2.8h1.6v6.9h-1.6Z", // seam
    "M10 4.2h2v3.2h-2Z",       // wheel
];
const BC: (f64, f64) = (16.2, 16.2);
// Charging is a bare bolt, not a disc with a bolt cut out: at 16 px Low (disc, 1 px "!") and Charging (disc, 1 px bolt) were
// the same 6 px blob (DAN-1472), and reading Low as Charging is the dangerous mistake. Round vs spiky outline instead.
const BOLT: &str = "M18.3 11.2L13.2 17.3H15.9L14.4 21.1L19.8 14.9H17.0Z";

/// The outline of the body as separate dashes, each a filled polygon (a stroke-dasharray would not recolour).
fn dashed_ring() -> Vec<String> {
    let (dash, gap, half, n) = (2.6_f64, 2.0_f64, 0.8_f64, 400_usize);
    let (x0, y0, x1, y1, r) = (6.0_f64, 2.0_f64, 16.0_f64, 20.0_f64, 5.0_f64);
    let mut pts: Vec<(f64, f64)> = Vec::new();
    let steps = n / 8;
    let mut arc = |cx: f64, cy: f64, a0: f64| {
        for i in 0..=steps {
            let a = (a0 + (90 * i) as f64 / steps as f64).to_radians();
            pts.push((cx + r * a.cos(), cy + r * a.sin()));
        }
    };
    arc(x1 - r, y0 + r, -90.0);
    arc(x1 - r, y1 - r, 0.0);
    arc(x0 + r, y1 - r, 90.0);
    arc(x0 + r, y0 + r, 180.0);
    pts.push(pts[0]);
    let mut seg = vec![0.0_f64];
    for w in pts.windows(2) {
        let last = *seg.last().expect("seg starts non-empty");
        seg.push(last + (w[1].0 - w[0].0).hypot(w[1].1 - w[0].1));
    }
    let total = *seg.last().expect("seg starts non-empty");
    // The point at arclength `s`, plus the segment's direction.
    let at = |s: f64| {
        let i = seg
            .iter()
            .position(|&v| v >= s)
            .unwrap_or(seg.len() - 1)
            .max(1);
        let span = seg[i] - seg[i - 1];
        let f = (s - seg[i - 1]) / if span == 0.0 { 1.0 } else { span };
        let (a, b) = (pts[i - 1], pts[i]);
        (a.0 + f * (b.0 - a.0), a.1 + f * (b.1 - a.1), a, b)
    };
    let mut out = Vec::new();
    let mut t = 0.0_f64;
    while t < total - 0.01 {
        let end = (t + dash).min(total);
        let (mut left, mut right) = (Vec::new(), Vec::new());
        let k = 8;
        for j in 0..=k {
            let (x, y, a, b) = at(t + (end - t) * j as f64 / k as f64);
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            let l = dx.hypot(dy);
            let l = if l == 0.0 { 1.0 } else { l };
            let (nx, ny) = (-dy / l, dx / l);
            left.push((x + nx * half, y + ny * half));
            right.push((x - nx * half, y - ny * half));
        }
        right.reverse();
        left.extend(right);
        let poly: Vec<String> = left.iter().map(|(x, y)| format!("{x:.2} {y:.2}")).collect();
        out.push(format!("M{}Z", poly.join("L")));
        t = end + gap;
    }
    out
}

/// One `<path>` per `(d, evenodd)`.
fn paths(parts: &[(String, bool)]) -> String {
    let mut s = String::new();
    for (d, evenodd) in parts {
        let rule = if *evenodd {
            r#" fill-rule="evenodd""#
        } else {
            ""
        };
        write!(s, r#"<path d="{d}"{rule}/>"#).expect("writing to a String");
    }
    s
}

fn svg(body: &[(String, bool)], badge: &[(String, bool)], clip: Option<String>) -> String {
    let (mut clip_def, mut body_attr) = (String::new(), "");
    if let Some(clip) = clip {
        clip_def = format!(
            r#"<clipPath id="knock"><path clip-rule="evenodd" d="M0 0H22V22H0Z{clip}"/></clipPath>"#
        );
        body_attr = r#" clip-path="url(#knock)""#;
    }
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 22 22" width="22" height="22">{STYLE}{clip_def}<g class="ColorScheme-Text" fill="currentColor"><g{body_attr}>{}</g>{}</g></svg>
"#,
        paths(body),
        paths(badge),
    )
}

/// `(file stem, file contents)` for each icon, in the order they are written.
pub fn all() -> Vec<(&'static str, String)> {
    // evenodd: 1.6-unit outline of (6,2)-(16,20) r5
    let ring = rrect(5.2, 1.2, 16.8, 20.8, 5.8) + &rrect(6.8, 2.8, 15.2, 19.2, 4.2);
    let mut mouse = vec![(ring, true)];
    mouse.extend(WHEEL.iter().map(|w| (w.to_string(), false)));
    let badge_clip = |r| Some(circle(BC.0, BC.1, r));
    vec![
        ("razerctl-symbolic", svg(&mouse, &[], None)),
        (
            "razerctl-nodevice-symbolic",
            svg(
                &dashed_ring()
                    .into_iter()
                    .map(|d| (d, false))
                    .collect::<Vec<_>>(),
                &[],
                None,
            ),
        ),
        (
            "razerctl-remap-symbolic",
            svg(&mouse, &[(circle(BC.0, BC.1, 2.9), false)], badge_clip(4.6)),
        ),
        (
            "razerctl-low-symbolic",
            svg(
                &mouse,
                &[(
                    circle(BC.0, BC.1, 4.4) + "M15.4 13.6h1.6v2.7h-1.6Z" + &circle(16.2, 18.4, 0.9),
                    true,
                )],
                badge_clip(5.8),
            ),
        ),
        (
            "razerctl-charging-symbolic",
            svg(&mouse, &[(BOLT.to_string(), false)], badge_clip(5.8)),
        ),
    ]
}

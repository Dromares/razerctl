//! Drift guard for the tray icons (DAN-1480): the SVGs committed under `packaging/icons/` must be byte-identical
//! to what the generator (`examples/gen_icons`, run with `cargo run --example gen_icons`) produces.

#[path = "../examples/gen_icons/icons.rs"]
mod icons;

use std::path::Path;

#[test]
fn committed_svgs_match_the_generator() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("packaging/icons");
    let icons = icons::all();
    assert_eq!(icons.len(), 5);
    for (name, generated) in icons {
        let path = dir.join(format!("{name}.svg"));
        let committed =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert!(
            committed == generated,
            "{name}.svg drifted from the generator; run `cargo run --example gen_icons` and commit the result"
        );
    }
}

#[test]
fn g_matches_printf_g() {
    // Expected strings are what `%g` prints (checked against the interpreter that wrote the committed SVGs).
    let cases: &[(f64, &str)] = &[
        (0.0, "0"),
        (-0.0, "-0"),
        (5.2 + 5.8, "11"),
        (6.8 - 1.6, "5.2"),
        (11.6 - 2.0 * 5.8, "0"),
        (0.1 + 0.2, "0.3"),
        (1.0 / 3.0, "0.333333"),
        (-2.0 / 3.0, "-0.666667"),
        (16.2 - 4.6, "11.6"),
        (123456.0, "123456"),
        (1234567.0, "1.23457e+06"),
        (999999.5, "1e+06"),
        (0.0001, "0.0001"),
        (0.00001234, "1.234e-05"),
        (100000.0, "100000"),
        (9.999995, "10"),
        (0.5, "0.5"),
        (2.5e-7, "2.5e-07"),
    ];
    for &(x, want) in cases {
        assert_eq!(icons::g(x), want, "g({x:?})");
    }
}

#[test]
fn fixed_two_decimals_round_like_printf() {
    // `%.2f` rounds the exact binary value half-to-even; 0.125 and 0.375 are exact ties.
    let cases: &[(f64, &str)] = &[
        (0.125, "0.12"),
        (0.375, "0.38"),
        (2.675, "2.67"), // binary value is just below the tie
        (1.005, "1.00"),
        (-0.001, "-0.00"),
        (10.994999, "10.99"),
        (11.0, "11.00"),
    ];
    for &(x, want) in cases {
        assert_eq!(format!("{x:.2}"), want, "{x:?}");
    }
}

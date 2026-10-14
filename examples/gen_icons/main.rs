//! Rewrites the five symbolic tray icons in `packaging/icons/` from the generator in `icons.rs`.
//!
//!     cargo run --example gen_icons
//!
//! Dev-time only: it is an example, so it is not built into the release binaries. `tests/icons.rs` fails
//! `cargo test` if the committed SVGs differ from what this writes.

mod icons;

use std::path::Path;

fn main() -> std::io::Result<()> {
    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("packaging/icons");
    for (name, text) in icons::all() {
        std::fs::write(out.join(format!("{name}.svg")), &text)?;
        println!("{name} {}", text.len());
    }
    Ok(())
}

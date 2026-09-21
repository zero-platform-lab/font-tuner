//! Demo CLI: render a sample string with each built-in profile to PNGs, and
//! (with `verify`) dump the greyscale + LCD blend curves for the C++ oracle
//! diff. See `verify/`.

use render_core::config::Profile;
use render_core::render::{render_text, Ink};
use render_core::{ft::Ft, tables_for};

const FONT: &str = r"C:\Windows\Fonts\meiryo.ttc";
const SAMPLE: &str = "水面に映る Rust — glyph 0123 あア亜";

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("verify") => { verify::dump(); return; }
        _ => {}
    }

    let ft = Ft::open(FONT, 0).expect("open font");
    let size = (760usize, 120usize);
    let pen = (24i32, 78i32);
    let px = 26;

    for (name, p) in [
        ("clean-greyscale.png", Profile::clean_greyscale()),
        ("clean-sharp.png", Profile::clean_sharp()),
        ("accurate.png", Profile::accurate()),
    ] {
        let t = tables_for(&p);
        let canvas = render_text(&ft, &t, &p, Ink::default(), SAMPLE, px, pen, size);
        canvas.save(name).expect("save png");
        println!("wrote {name}");
    }
}

/// Curve dumps used by verify/compare.py against the C++ oracle.
mod verify {
    use render_core::Tables;

    fn dump_gray() {
        let cases: &[(&str, f32, f32, f32, i32)] = &[
            ("g125", 1.25, 1.0, 1.0, 0),
            ("linear", 1.0, 1.0, 1.0, -1),
            ("g13", 1.30, 1.0, 1.0, 0),
            ("w115c14", 1.25, 1.15, 1.4, 0),
            ("srgb", 1.0, 1.0, 1.0, 1),
            ("mode2", 1.0, 1.0, 1.0, 2),
        ];
        for (tag, g, w, c, m) in cases {
            let t = Tables::build(*g, *w, *c, *m);
            let line: String = (0..=255u8)
                .map(|cov| t.blend(255, 0, cov).to_string())
                .collect::<Vec<_>>()
                .join(" ");
            std::fs::write(format!("rust-{tag}.txt"), line).unwrap();
        }
    }

    fn dump_lcd() {
        let t = Tables::build(1.25, 1.0, 1.0, 0);
        let bgs = [[255u8, 255, 255], [128, 128, 128], [200, 100, 50]];
        let covs = [0u8, 1, 64, 128, 200, 255];
        let mut out = String::new();
        for aamode in 2..=3 {
            for bg in &bgs {
                for &p0 in &covs {
                    for &p1 in &covs {
                        for &p2 in &covs {
                            let (a_r, a_g, a_b) = if aamode == 2 { (p0, p1, p2) } else { (p2, p1, p0) };
                            let r = t.blend(bg[0], 0, a_b);
                            let g = t.blend(bg[1], 0, a_g);
                            let b = t.blend(bg[2], 0, a_r);
                            out.push_str(&format!("{r},{g},{b} "));
                        }
                    }
                }
            }
        }
        std::fs::write("lcd-rust.txt", out.trim_end()).unwrap();
    }

    pub fn dump() {
        dump_gray();
        dump_lcd();
        println!("wrote rust-*.txt, lcd-rust.txt");
    }
}

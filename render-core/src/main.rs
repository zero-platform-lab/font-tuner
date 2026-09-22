//! Demo CLI: render a sample string with each built-in profile to PNGs.

use render_core::config::Profile;
use render_core::render::{render_text, Ink};
use render_core::{ft::Ft, tables_for};

const FONT: &str = r"C:\Windows\Fonts\meiryo.ttc";
const SAMPLE: &str = "水面に映る Rust — glyph 0123 あア亜";

fn main() {
    const WHITE: [u8; 3] = [255, 255, 255];
    const DARK: [u8; 3] = [30, 30, 30];
    const LIGHT_INK: Ink = Ink { fg: [222, 222, 222] };
    const BLUE_INK: Ink = Ink { fg: [40, 90, 200] };

    let ft = Ft::open(FONT, 0).expect("open font");
    let size = (760usize, 120usize);
    let pen = (24i32, 78i32);
    let px = 26;

    // light profiles, black text on white
    let light = [
        ("clean-greyscale.png", Profile::clean_greyscale()),
        ("clean-sharp.png", Profile::clean_sharp()),
        ("accurate.png", Profile::accurate()),
    ];
    for (name, p) in light {
        let t = tables_for(&p);
        let canvas = render_text(&ft, &t, &p, Ink::default(), WHITE, SAMPLE, px, pen, size);
        canvas.save(name).expect("save png");
        println!("wrote {name}");
    }

    // dark profiles, light text on dark
    let dark = [
        ("clean-dark-greyscale.png", Profile::clean_dark_greyscale()),
        ("clean-sharp-dark.png", Profile::clean_sharp_dark()),
    ];
    for (name, p) in dark {
        let t = tables_for(&p);
        let canvas = render_text(&ft, &t, &p, LIGHT_INK, DARK, SAMPLE, px, pen, size);
        canvas.save(name).expect("save png");
        println!("wrote {name}");
    }

    // coloured text (greyscale path)
    let p = Profile::clean_greyscale();
    let t = tables_for(&p);
    let canvas = render_text(&ft, &t, &p, BLUE_INK, WHITE, SAMPLE, px, pen, size);
    canvas.save("colour-blue.png").expect("save png");
    println!("wrote colour-blue.png");
}

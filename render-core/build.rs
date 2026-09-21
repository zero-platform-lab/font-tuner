// Link the FreeType fork's static lib (build/lib/freetype64.lib, produced by
// build-core.ps1 at the repo root). The bindings live in src/ft.rs; there is
// no C in this crate.
use std::path::Path;

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let repo = Path::new(&manifest).parent().expect("repo root");
    println!(
        "cargo:rustc-link-search=native={}",
        repo.join("build").join("lib").display()
    );
    println!("cargo:rustc-link-lib=static=freetype64");
}

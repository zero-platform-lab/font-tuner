// Compile the FreeType C shim (shim.c) against the fork's headers and link the
// static fork lib. Paths are resolved relative to the repo root so the crate is
// portable within the checkout. `freetype64.lib` must exist first — run
// build-core.ps1 at the repo root (it produces build/lib/freetype64.lib).
use std::path::Path;

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let repo = Path::new(&manifest).parent().expect("repo root");

    cc::Build::new()
        .file("shim.c")
        .include(repo.join("vendor").join("freetype").join("include"))
        .compile("ftshim");

    println!(
        "cargo:rustc-link-search=native={}",
        repo.join("build").join("lib").display()
    );
    println!("cargo:rustc-link-lib=static=freetype64");
    println!("cargo:rerun-if-changed=shim.c");
}

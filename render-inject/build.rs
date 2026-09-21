// Pin `GetMsgProc` to the first byte of `.text` (RVA 0x1000) with link.exe's
// `/ORDER`. The tray hooks with `hmod + rva`, and a process that still holds an
// older build of this DLL gets `old_base + rva` — so the RVA must never move
// between builds. See the doc comment on `GetMsgProc` in `src/lib.rs`.
fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rerun-if-changed=order.txt");
    println!("cargo:rustc-cdylib-link-arg=/ORDER:@{dir}/order.txt");
}

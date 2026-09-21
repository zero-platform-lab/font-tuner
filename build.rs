// Embed the tray/app icons (app.rc) into the executable.
fn main() {
    let _ = embed_resource::compile("app.rc", embed_resource::NONE);
}

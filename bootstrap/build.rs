// Embed the version resource into RenderBootstrap64.dll.
fn main() {
    version_rc(env!("CARGO_PKG_VERSION"), "RenderBootstrap64.dll", "font-tuner bootstrap", 2, true);
}

/// Emit a VERSIONINFO resource for `file` and hand it to embed-resource.
/// Windows Installer replaces an existing file only if the new one carries a
/// higher version; an unversioned file that looks "modified" (mtime != ctime)
/// is left alone, which silently kept an old RenderBootstrap64.dll in place on upgrade.
fn version_rc(version: &str, file: &str, description: &str, filetype: u32, cdylib: bool) {
    let out = std::env::var("OUT_DIR").unwrap();
    let mut n = version.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let (a, b, c) = (n.next().unwrap_or(0), n.next().unwrap_or(0), n.next().unwrap_or(0));
    let rc = format!(
        r#"1 VERSIONINFO
FILEVERSION {a},{b},{c},0
PRODUCTVERSION {a},{b},{c},0
FILEFLAGSMASK 0x3f
FILEFLAGS 0
FILEOS 0x40004
FILETYPE {filetype}
FILESUBTYPE 0
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "CompanyName", "zero-platform-Lab"
      VALUE "FileDescription", "{description}"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "{file}"
      VALUE "LegalCopyright", "GPL-3.0-only"
      VALUE "OriginalFilename", "{file}"
      VALUE "ProductName", "font-tuner"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );
    let path = format!("{out}/version.rc");
    std::fs::write(&path, rc).unwrap();
    if cdylib {
        embed_resource::compile_for_cdylib(&path, embed_resource::NONE).manifest_optional().unwrap();
    } else {
        embed_resource::compile(&path, embed_resource::NONE).manifest_optional().unwrap();
    }
}

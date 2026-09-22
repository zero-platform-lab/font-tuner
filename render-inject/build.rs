// Pin `GetMsgProc` to the first byte of `.text` (RVA 0x1000) with link.exe's
// `/ORDER`. The tray hooks with `hmod + rva`, and a process that still holds an
// older build of this DLL gets `old_base + rva` — so the RVA must never move
// between builds. See the doc comment on `GetMsgProc` in `src/lib.rs`.
//
// The version resource takes the shipped version from the root Cargo.toml
// (`[workspace.package] version`): this crate is outside the workspace, so it
// cannot inherit it. Resources live in `.rsrc`, not `.text`, so the RVA pin is
// unaffected (build-msi.ps1 checks anyway).
fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rerun-if-changed=order.txt");
    println!("cargo:rustc-cdylib-link-arg=/ORDER:@{dir}/order.txt");

    let root = format!("{dir}/../Cargo.toml");
    println!("cargo:rerun-if-changed={root}");
    let version = workspace_version(&std::fs::read_to_string(&root).expect("read root Cargo.toml"));
    version_rc(&version, "RenderCore64.dll", "font-tuner render core", 2, true);
}

/// `version = "..."` under `[workspace.package]` of the root manifest.
fn workspace_version(toml: &str) -> String {
    let mut in_section = false;
    for line in toml.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line == "[workspace.package]";
        } else if in_section {
            if let Some(rest) = line.strip_prefix("version") {
                if let Some(v) = rest.trim_start().strip_prefix('=') {
                    return v.trim().trim_matches('"').to_string();
                }
            }
        }
    }
    panic!("[workspace.package] version not found in root Cargo.toml");
}

/// Emit a VERSIONINFO resource for `file` and hand it to embed-resource.
/// Windows Installer replaces an existing file only if the new one carries a
/// higher version; an unversioned file that looks "modified" (mtime != ctime)
/// is left alone, which silently kept an old RenderCore64.dll in place on upgrade.
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

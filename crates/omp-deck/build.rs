//! Windows: embeds the application icon and version information into the `.exe`.
//! Other targets: nothing to do.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let Some(out_dir) = std::env::var_os("OUT_DIR").map(PathBuf::from) else {
        println!("cargo:warning=OUT_DIR is not set: the Windows resource is skipped");
        return;
    };
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let icon = PathBuf::from(&manifest).join("assets").join("icon.ico");
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_owned());
    let mut parts = version
        .split(['.', '-', '+'])
        .map(|p| p.parse::<u16>().unwrap_or(0));
    let mut next = || parts.next().unwrap_or(0);
    let (major, minor, patch) = (next(), next(), next());
    // rc.exe wants forward slashes (or escaped backslashes) in string literals.
    let icon = icon.display().to_string().replace('\\', "/");
    let rc = format!(
        "1 ICON \"{icon}\"\n\
         1 VERSIONINFO\n\
         FILEVERSION {major},{minor},{patch},0\n\
         PRODUCTVERSION {major},{minor},{patch},0\n\
         BEGIN\n\
           BLOCK \"StringFileInfo\"\n\
           BEGIN\n\
             BLOCK \"040904b0\"\n\
             BEGIN\n\
               VALUE \"FileDescription\", \"OmpDeck\"\n\
               VALUE \"ProductName\", \"OmpDeck\"\n\
               VALUE \"InternalName\", \"omp-deck\"\n\
               VALUE \"OriginalFilename\", \"omp-deck.exe\"\n\
               VALUE \"FileVersion\", \"{version}\"\n\
               VALUE \"ProductVersion\", \"{version}\"\n\
             END\n\
           END\n\
           BLOCK \"VarFileInfo\"\n\
           BEGIN\n\
             VALUE \"Translation\", 0x409, 1200\n\
           END\n\
         END\n"
    );
    let rc_path = out_dir.join("omp-deck.rc");
    if let Err(e) = std::fs::write(&rc_path, rc) {
        println!(
            "cargo:warning=writing {}: {e}; the Windows resource is skipped",
            rc_path.display()
        );
        return;
    }
    let outcome = embed_resource::compile(&rc_path, embed_resource::NONE);
    // A Windows target built from another OS (cargo xwin) may lack a resource compiler; only a
    // native Windows build must have one.
    if let Err(e) = outcome.manifest_optional() {
        if cfg!(windows) {
            eprintln!("embedding the Windows resource failed: {e}");
            std::process::exit(1);
        }
        println!("cargo:warning=embedding the Windows resource failed: {e}");
    }
}

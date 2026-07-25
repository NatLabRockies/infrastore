use std::env;
use std::path::PathBuf;

fn main() {
    let crate_dir = env::var("CARGO_MANIFEST_DIR").unwrap();

    println!("cargo:rerun-if-changed=src/lib.rs");
    println!("cargo:rerun-if-changed=cbindgen.toml");

    // Regenerating the header is a development convenience for this workspace.
    // When the crate is consumed from crates.io it is built out of Cargo's
    // registry cache, where `include/` is read-only and the shipped
    // `infrastore.h` is already the committed one -- so skip generation there.
    // `Cargo.toml.orig` exists only in an unpacked `.crate`, which makes it a
    // reliable marker for that case.
    if PathBuf::from(&crate_dir).join("Cargo.toml.orig").exists() {
        return;
    }

    let out_path = PathBuf::from(&crate_dir)
        .join("include")
        .join("infrastore.h");
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    // cbindgen is a best-effort dev convenience. If it fails (e.g. on a build
    // without all features), don't break the build — just print a warning.
    match cbindgen::Builder::new()
        .with_crate(&crate_dir)
        .with_config(
            cbindgen::Config::from_file(format!("{crate_dir}/cbindgen.toml")).unwrap_or_default(),
        )
        .generate()
    {
        Ok(bindings) => {
            bindings.write_to_file(&out_path);
        }
        Err(e) => {
            println!("cargo:warning=cbindgen failed: {e}");
        }
    }
}

use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo::rerun-if-changed=Cargo.toml");

    let package_major = env::var("CARGO_PKG_VERSION_MAJOR")
        .expect("Cargo must provide CARGO_PKG_VERSION_MAJOR")
        .parse::<u64>()
        .expect("Cargo package major version must fit in u64");
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo must provide OUT_DIR"))
        .join("package_version.rs");
    fs::write(
        output,
        format!("pub(crate) const PACKAGE_MAJOR: u64 = {package_major};\n"),
    )
    .expect("package version module must be writable");
}

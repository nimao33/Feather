fn main() {
    // Build scripts are compiled for the host, so `cfg!(windows)` would be
    // true while cross-compiling to Linux. Ask Cargo about the target instead.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    println!("cargo:rerun-if-changed=feather.manifest");
    println!("cargo:rerun-if-changed=feather.rc");
    println!("cargo:rerun-if-changed=feather.ico");
    // The version resource reads the version from Cargo.toml.
    let version = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let mut parts: Vec<u16> = version
        .split(['.', '-', '+'])
        .take(3)
        .map(|part| part.parse().unwrap_or(0))
        .collect();
    parts.resize(4, 0);
    let numbers = parts
        .iter()
        .map(|part| part.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let macros = [
        format!("FEATHER_VERSION={numbers}"),
        format!("FEATHER_VERSION_TEXT=\"{version}\""),
    ];
    embed_resource::compile("feather.rc", &macros)
        .manifest_required()
        .expect("embed the application manifest");
}

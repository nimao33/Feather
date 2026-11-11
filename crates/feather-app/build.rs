fn main() {
    // Build scripts are compiled for the host, so `cfg!(windows)` would be
    // true while cross-compiling to Linux. Ask Cargo about the target instead.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    println!("cargo:rerun-if-changed=feather.manifest");
    println!("cargo:rerun-if-changed=feather.rc");
    println!("cargo:rerun-if-changed=feather.ico");
    embed_resource::compile("feather.rc", embed_resource::NONE)
        .manifest_required()
        .expect("embed the application manifest");
}

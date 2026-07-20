use std::{env, path::PathBuf};

fn main() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("Cargo manifest directory"));
    let resource = manifest.join("../../assets/windows/winrisef-toolbox-agent.res");
    println!("cargo:rerun-if-changed={}", resource.display());
    println!(
        "cargo:rustc-link-arg-bin=winrisef-agent={}",
        resource.canonicalize().expect("Windows resource must exist").display()
    );
}

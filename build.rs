// Record the compilation target so `fridica build-info` identifies every build,
// including wheels built without the candidate packager's environment.
fn main() {
    println!("cargo:rerun-if-env-changed=FRIDICA_BUILD_TARGET");
    println!("cargo:rerun-if-env-changed=FRIDICA_BUILD_ID");
    let target = std::env::var("TARGET").unwrap_or_default();
    println!("cargo:rustc-env=FRIDICA_COMPILE_TARGET={target}");
}

fn main() {
    let root =
        std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("../..");
    let output =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("build-info.json");
    for input in [
        "crates",
        "model-manifests",
        ".cargo",
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        ".github/release.py",
    ] {
        println!("cargo:rerun-if-changed={}", root.join(input).display());
    }
    println!("cargo:rerun-if-env-changed=SHERPA_ONNX_LIB_DIR");
    for variable in [
        "RUSTC",
        "RUSTC_LINKER",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "RUSTUP_TOOLCHAIN",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_HOME",
        "PATH",
        "CARGO_BUILD_RUSTFLAGS",
        "CARGO_BUILD_RUSTC",
        "CARGO_BUILD_RUSTC_WRAPPER",
        "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
        "CARGO_BUILD_TARGET",
        "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS",
        "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER",
        "CC",
        "CXX",
        "CFLAGS",
        "CXXFLAGS",
        "VCToolsInstallDir",
    ] {
        println!("cargo:rerun-if-env-changed={variable}");
    }
    if let Some(native) = std::env::var_os("SHERPA_ONNX_LIB_DIR") {
        println!(
            "cargo:rerun-if-changed={}",
            std::path::PathBuf::from(native).display()
        );
    }
    let status = std::process::Command::new("python")
        .arg(root.join(".github/release.py"))
        .arg("--emit-build-info")
        .arg(output)
        .arg("--cargo-build-context")
        .arg("--target")
        .arg(std::env::var("TARGET").unwrap())
        .arg("--profile")
        .arg(std::env::var("PROFILE").unwrap())
        .status()
        .expect("Run build provenance generator (Python 3.11+ required)");
    assert!(status.success(), "Generate verified build provenance");
    println!("cargo:rerun-if-changed=assets/app-icon.rc");
    println!("cargo:rerun-if-changed=assets/app-icon.ico");
    embed_resource::compile("assets/app-icon.rc", embed_resource::NONE)
        .manifest_required()
        .expect("Compile application icon resources");
}

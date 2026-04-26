extern crate cmake;

use cmake::Config;
use std::{env, path::PathBuf, process::Command};

/// True when Cargo is cross-compiling for an iOS device or simulator.
fn target_is_ios() -> bool {
    env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("ios")
}

/// True when the iOS target is the simulator (aarch64-apple-ios-sim or
/// x86_64-apple-ios). Cargo sets `CARGO_CFG_TARGET_ABI=sim` for the modern
/// arm64 simulator triple, and the legacy x86_64-apple-ios triple is
/// simulator-only on Apple Silicon dev toolchains.
fn target_is_ios_simulator() -> bool {
    if !target_is_ios() {
        return false;
    }
    if env::var("CARGO_CFG_TARGET_ABI").as_deref() == Ok("sim") {
        return true;
    }
    // x86_64-apple-ios is always the simulator (no x86_64 iOS hardware exists).
    env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86_64")
}

/// Locate the Xcode clang runtime directory for macOS builds.
///
/// Returns the `lib/darwin/` path that contains `libclang_rt.osx.a`.
/// Only called on macOS — on iOS the `___isPlatformVersionAtLeast` symbol is
/// provided by the system runtime so no explicit link is needed, and trying to
/// link the fat `libclang_rt.ios.a` causes a build error on cross-compiles.
fn find_clang_rt_macos() -> Option<String> {
    let output = Command::new("xcode-select")
        .args(["--print-path"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let developer_dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let toolchain_base = format!(
        "{}/Toolchains/XcodeDefault.xctoolchain/usr/lib/clang",
        developer_dir
    );

    let clang_dir = std::fs::read_dir(&toolchain_base).ok()?;
    for entry in clang_dir.flatten() {
        let darwin_path = entry.path().join("lib/darwin");
        if darwin_path.join("libclang_rt.osx.a").exists() {
            return Some(darwin_path.to_string_lossy().to_string());
        }
    }

    None
}

fn build_and_link_mlx_c() {
    let is_ios = target_is_ios();
    let is_ios_sim = target_is_ios_simulator();
    let mut config = Config::new("src/mlx-c");
    config.very_verbose(true);
    config.define("CMAKE_INSTALL_PREFIX", ".");
    // mlx-c's examples link against tools that don't cross-compile cleanly
    // (they assume host architecture). The Rust bindings don't need them.
    config.define("MLX_C_BUILD_EXAMPLES", "OFF");

    if is_ios {
        // Cross-compile for an iOS device or simulator.
        // Setting CMAKE_SYSTEM_NAME=iOS makes CMake select the right SDK
        // based on CMAKE_OSX_SYSROOT; we pin the sysroot explicitly via
        // xcrun for hermeticity.
        let sdk_name = if is_ios_sim { "iphonesimulator" } else { "iphoneos" };
        let sdk_path = Command::new("xcrun")
            .args(["--sdk", sdk_name, "--show-sdk-path"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .unwrap_or_default();
        let sdk_path = sdk_path.trim().to_string();

        // CMake uses "iOS" for both device and simulator; the sysroot
        // distinguishes them. The arch is determined by the Cargo target.
        let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_else(|_| "arm64".into());
        let cmake_arch = match arch.as_str() {
            "aarch64" => "arm64",
            other => other,
        };

        config.define("CMAKE_SYSTEM_NAME", "iOS");
        config.define("CMAKE_OSX_ARCHITECTURES", cmake_arch);
        // MLX uses C++17 standard library features that require iOS 16+.
        config.define("CMAKE_OSX_DEPLOYMENT_TARGET", "16.0");
        if !sdk_path.is_empty() {
            config.define("CMAKE_OSX_SYSROOT", &sdk_path);
        }
        // The simulator runs on the host (Mac); arm64 simulator on Apple
        // Silicon supports Metal, but we keep Accelerate ON for both since
        // it ships in the simulator SDK as well.
        config.define("MLX_BUILD_METAL", "ON");
        config.define("MLX_BUILD_ACCELERATE", "ON");
    } else {
        // Native macOS build — pin Xcode's clang for SDK compatibility.
        config.define("CMAKE_C_COMPILER", "/usr/bin/cc");
        config.define("CMAKE_CXX_COMPILER", "/usr/bin/c++");

        config.define("MLX_BUILD_METAL", "OFF");
        config.define("MLX_BUILD_ACCELERATE", "OFF");

        #[cfg(feature = "metal")]
        {
            config.define("MLX_BUILD_METAL", "ON");
        }

        #[cfg(feature = "accelerate")]
        {
            config.define("MLX_BUILD_ACCELERATE", "ON");
        }
    }

    #[cfg(debug_assertions)]
    {
        config.define("CMAKE_BUILD_TYPE", "Debug");
    }

    #[cfg(not(debug_assertions))]
    {
        config.define("CMAKE_BUILD_TYPE", "Release");
    }

    let dst = config.build();

    println!("cargo:rustc-link-search=native={}/build/lib", dst.display());
    println!("cargo:rustc-link-lib=static=mlx");
    println!("cargo:rustc-link-lib=static=mlxc");

    println!("cargo:rustc-link-lib=c++");
    println!("cargo:rustc-link-lib=dylib=objc");
    println!("cargo:rustc-link-lib=framework=Foundation");

    if is_ios {
        // On iOS Metal and Accelerate are always compiled in (see CMake flags above).
        println!("cargo:rustc-link-lib=framework=Metal");
        println!("cargo:rustc-link-lib=framework=Accelerate");
    } else {
        #[cfg(feature = "metal")]
        {
            println!("cargo:rustc-link-lib=framework=Metal");
        }

        #[cfg(feature = "accelerate")]
        {
            println!("cargo:rustc-link-lib=framework=Accelerate");
        }

        // Link the Xcode clang runtime for `___isPlatformVersionAtLeast`.
        // Needed on macOS 26+ where the bundled LLVM runtime may be outdated.
        // See: https://github.com/conda-forge/llvmdev-feedstock/issues/244
        // Not needed on iOS — the system runtime provides the symbol, and the
        // fat libclang_rt.ios.a causes a build error in Rust's cross-compile.
        if let Some(clang_rt_path) = find_clang_rt_macos() {
            println!("cargo:rustc-link-search={}", clang_rt_path);
            println!("cargo:rustc-link-lib=static=clang_rt.osx");
        }
    }
}

fn main() {
    build_and_link_mlx_c();

    let bindings = bindgen::Builder::default()
        .rust_target("1.73.0".parse().expect("rust-version"))
        .header("src/mlx-c/mlx/c/mlx.h")
        .header("src/mlx-c/mlx/c/linalg.h")
        .header("src/mlx-c/mlx/c/error.h")
        .header("src/mlx-c/mlx/c/transforms_impl.h")
        .clang_arg("-Isrc/mlx-c")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("Unable to generate bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");
}

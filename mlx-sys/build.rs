extern crate cmake;

use cmake::Config;
use std::{env, path::PathBuf, process::Command};

/// True when Cargo is cross-compiling for an iOS device (aarch64-apple-ios).
fn target_is_ios() -> bool {
    env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("ios")
}

/// Locate the Xcode clang runtime directory and return `(search_dir, link_name)`.
///
/// Both `libclang_rt.osx.a` (macOS) and `libclang_rt.ios.a` (iOS) live in the
/// same `…/XcodeDefault.xctoolchain/…/lib/darwin/` directory, so we scan once
/// and choose the right library name based on the target.
///
/// This symbol is needed for `___isPlatformVersionAtLeast` on macOS 26+ and for
/// platform-version guards in MLX's C++ runtime on iOS.
fn find_clang_rt() -> Option<(String, &'static str)> {
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

    // Both osx and ios archives sit in the same `darwin/` subdirectory; pick
    // the one that matches our target so the linker gets iOS slices for iOS.
    let (lib_file, link_name): (&str, &str) = if target_is_ios() {
        ("libclang_rt.ios.a", "clang_rt.ios")
    } else {
        ("libclang_rt.osx.a", "clang_rt.osx")
    };

    let clang_dir = std::fs::read_dir(&toolchain_base).ok()?;
    for entry in clang_dir.flatten() {
        let darwin_path = entry.path().join("lib/darwin");
        if darwin_path.join(lib_file).exists() {
            return Some((darwin_path.to_string_lossy().to_string(), link_name));
        }
    }

    None
}

fn build_and_link_mlx_c() {
    let is_ios = target_is_ios();
    let mut config = Config::new("src/mlx-c");
    config.very_verbose(true);
    config.define("CMAKE_INSTALL_PREFIX", ".");

    if is_ios {
        // Cross-compile for an iOS device (arm64).
        // Setting CMAKE_SYSTEM_NAME=iOS causes CMake to select the iphoneos SDK
        // automatically; we pin the sysroot explicitly for hermeticity.
        let sdk_path = Command::new("xcrun")
            .args(["--sdk", "iphoneos", "--show-sdk-path"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .unwrap_or_default();
        let sdk_path = sdk_path.trim().to_string();

        config.define("CMAKE_SYSTEM_NAME", "iOS");
        config.define("CMAKE_OSX_ARCHITECTURES", "arm64");
        // MLX uses C++17 standard library features that require iOS 16+.
        config.define("CMAKE_OSX_DEPLOYMENT_TARGET", "16.0");
        if !sdk_path.is_empty() {
            config.define("CMAKE_OSX_SYSROOT", &sdk_path);
        }
        // Metal and Accelerate are always available on iOS devices; the CPU
        // fallback paths MLX uses on macOS don't apply to iOS.
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
    }

    // Link the Xcode clang runtime for `___isPlatformVersionAtLeast`.
    if let Some((search_dir, lib_name)) = find_clang_rt() {
        println!("cargo:rustc-link-search={}", search_dir);
        println!("cargo:rustc-link-lib=static={}", lib_name);
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

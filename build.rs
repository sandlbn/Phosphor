fn main() {
    if std::env::var_os("CARGO_FEATURE_AIRPLAY").is_some() {
        build_airplay();
    }

    #[cfg(target_os = "windows")]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/phosphor.ico");

        // Embed version-info so the signed exe, UAC prompt, and
        // Add/Remove Programs "Publisher" are coherent with the code-signing
        // certificate subject. Version fields come from Cargo at build time.
        let version = env!("CARGO_PKG_VERSION");
        res.set("CompanyName", "Marcin Spoczynski");
        res.set("ProductName", "Phosphor");
        res.set(
            "FileDescription",
            "Phosphor — a SID player for USBSID-Pico / Ultimate 64",
        );
        res.set("ProductVersion", version);
        res.set("FileVersion", version);
        res.set("LegalCopyright", "Copyright © Marcin Spoczynski");
        res.set("OriginalFilename", "phosphor.exe");

        res.compile().expect("Failed to compile Windows resources");
    }
}

/// Build the vendored AirPlay sender (C++20 + Mbed TLS) and our C shim,
/// then link them statically.
fn build_airplay() {
    println!("cargo:rerun-if-changed=native/airplay");
    println!("cargo:rerun-if-changed=vendor/airplay2-sender-cpp/src");
    if !std::path::Path::new("vendor/airplay2-sender-cpp/CMakeLists.txt").exists() {
        panic!(
            "AirPlay sources missing: run `git submodule update --init` \
             (or build with --no-default-features --features usb)"
        );
    }
    // Match Rust's C runtime. Windows builds use `+crt-static`
    // (.cargo/config.toml) so phosphor.exe needs no VC++ redistributable;
    // the C++ side must then use /MT too, or the link fails with LNK2038
    // RuntimeLibrary mismatches. CMP0091 makes CMAKE_MSVC_RUNTIME_LIBRARY
    // apply to fetched sub-projects (Mbed TLS) that declare an older
    // cmake_minimum_required.
    let static_crt = std::env::var("CARGO_CFG_TARGET_FEATURE")
        .map(|f| f.split(',').any(|x| x == "crt-static"))
        .unwrap_or(false);
    let mut cfg = cmake::Config::new("native/airplay");
    cfg.profile("Release").build_target("phosphor_airplay");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        cfg.static_crt(static_crt)
            .define("CMAKE_POLICY_DEFAULT_CMP0091", "NEW")
            .define(
                "CMAKE_MSVC_RUNTIME_LIBRARY",
                if static_crt {
                    "MultiThreaded"
                } else {
                    "MultiThreadedDLL"
                },
            );
    }
    let dst = cfg.build();
    let b = dst.join("build");
    // Multi-config generators (MSVC) put libraries in a per-config subdir.
    for dir in [
        b.clone(),
        b.join("Release"),
        b.join("airplay2"),
        b.join("airplay2").join("Release"),
        b.join("_deps/mbedtls-build/library"),
        b.join("_deps/mbedtls-build/library/Release"),
        b.join("_deps/mbedtls-build/3rdparty/p256-m"),
        b.join("_deps/mbedtls-build/3rdparty/p256-m/Release"),
        b.join("_deps/mbedtls-build/3rdparty/everest"),
        b.join("_deps/mbedtls-build/3rdparty/everest/Release"),
    ] {
        println!("cargo:rustc-link-search=native={}", dir.display());
    }
    for lib in [
        "phosphor_airplay",
        "raop_sender",
        "airplay_crypto",
        "ed25519",
        "mbedcrypto",
        "p256m",
        "everest",
    ] {
        println!("cargo:rustc-link-lib=static={lib}");
    }
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("apple") {
        println!("cargo:rustc-link-lib=dylib=c++");
    } else if target.contains("windows") {
        println!("cargo:rustc-link-lib=dylib=ws2_32");
        println!("cargo:rustc-link-lib=dylib=bcrypt");
    } else {
        println!("cargo:rustc-link-lib=dylib=stdc++");
    }
}

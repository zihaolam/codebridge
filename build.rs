use std::env;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Command;

fn generate_web_assets(root: &std::path::Path) {
    let dist = root.join("internal/web/dist");
    println!("cargo:rerun-if-changed={}", dist.display());
    let mut files = Vec::new();
    fn visit(base: &std::path::Path, dir: &std::path::Path, out: &mut Vec<(String, PathBuf)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(base, &path, out);
            } else if path.file_name().is_some_and(|name| name != ".gitkeep") {
                let relative = path
                    .strip_prefix(base)
                    .expect("web asset under dist")
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((relative, path));
            }
        }
    }
    visit(&dist, &dist, &mut files);
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let mut generated = String::from(
        "pub fn embedded_asset(path: &str) -> Option<&'static [u8]> {\n    match path {\n",
    );
    for (relative, absolute) in files {
        writeln!(
            generated,
            "        {:?} => Some(include_bytes!({:?})),",
            relative,
            absolute.to_string_lossy()
        )
        .expect("write generated asset match");
    }
    generated.push_str("        _ => None,\n    }\n}\n");
    let out = PathBuf::from(env::var("OUT_DIR").expect("cargo out dir"));
    std::fs::write(out.join("web_assets.rs"), generated).expect("write web asset module");
}

fn zig_target(target: &str) -> &str {
    match target {
        "x86_64-unknown-linux-gnu" => "x86_64-linux-gnu",
        "aarch64-unknown-linux-gnu" => "aarch64-linux-gnu",
        "x86_64-unknown-linux-musl" => "x86_64-linux-musl",
        "aarch64-unknown-linux-musl" => "aarch64-linux-musl",
        "x86_64-apple-darwin" => "x86_64-macos",
        "aarch64-apple-darwin" => "aarch64-macos",
        "x86_64-pc-windows-msvc" => "x86_64-windows-msvc",
        "aarch64-pc-windows-msvc" => "aarch64-windows-msvc",
        other => panic!("unsupported target for libghostty-vt: {other}"),
    }
}

fn main() {
    if let Ok(output) = Command::new(env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
        .arg("--version")
        .output()
    {
        if output.status.success() {
            let version = String::from_utf8_lossy(&output.stdout);
            println!("cargo:rustc-env=RUSTC_VERSION={}", version.trim());
        }
    }
    for path in [
        "build.rs",
        "vendor/libghostty-vt.vendor.json",
        "vendor/libghostty-vt/build.zig",
        "vendor/libghostty-vt/build.zig.zon",
        "vendor/libghostty-vt/include",
        "vendor/libghostty-vt/pkg",
        "vendor/libghostty-vt/src",
        "vendor/libghostty-vt/VERSION",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    for variable in ["LIBGHOSTTY_VT_OPTIMIZE", "LIBGHOSTTY_VT_SIMD", "ZIG"] {
        println!("cargo:rerun-if-env-changed={variable}");
    }

    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest directory"));
    generate_web_assets(&root);
    let vendored = root.join("vendor/libghostty-vt");
    let target = env::var("TARGET").expect("cargo target");
    let version = std::fs::read_to_string(vendored.join("VERSION"))
        .expect("vendored libghostty-vt VERSION")
        .trim()
        .to_owned();
    let optimize = env::var("LIBGHOSTTY_VT_OPTIMIZE").unwrap_or_else(|_| "ReleaseFast".to_owned());
    let simd = env::var("LIBGHOSTTY_VT_SIMD").unwrap_or_else(|_| "true".to_owned());

    let local_zig = root
        .join(".tools")
        .join(format!("zig-{}-0.15.2", zig_target(&target)))
        .join(if target.contains("windows") {
            "zig.exe"
        } else {
            "zig"
        });
    let zig = env::var_os("ZIG")
        .map(PathBuf::from)
        .or_else(|| local_zig.is_file().then_some(local_zig))
        .unwrap_or_else(|| PathBuf::from("zig"));
    let status = Command::new(zig)
        .current_dir(&vendored)
        .args([
            "build",
            "-Demit-lib-vt",
            &format!("-Doptimize={optimize}"),
            &format!("-Dsimd={simd}"),
            &format!("-Dtarget={}", zig_target(&target)),
            &format!("-Dversion-string={version}"),
            "-Demit-xcframework=false",
        ])
        .status()
        .expect("execute zig for vendored libghostty-vt");
    assert!(status.success(), "vendored libghostty-vt build failed");

    let lib_dir = vendored.join("zig-out/lib");
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    if target.contains("apple-darwin") {
        println!(
            "cargo:rustc-link-arg={}",
            lib_dir.join("libghostty-vt.a").display()
        );
    } else if target.contains("windows-msvc") {
        println!("cargo:rustc-link-lib=static=ghostty-vt-static");
    } else {
        println!("cargo:rustc-link-lib=static=ghostty-vt");
    }
}

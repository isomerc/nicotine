// Build script: generate a multi-size Windows ICO from assets/icon.png
// and embed it as the executable's icon resource so Explorer shows the
// Nicotine logo on nicotine.exe. Only runs when the target is Windows.
//
// We do the PNG→ICO conversion in Rust (image + ico crates) so there's
// no external image tool required on the host. For the .res compile we
// look for `rc.exe` (MSVC native), `windres` (mingw), or `llvm-rc` /
// `llvm-rc-N` (LLVM / cargo-xwin setups). On Linux cross-builds with
// cargo-xwin the llvm-rc path is the usual one, since rc.exe isn't on
// $PATH.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.png");
    println!("cargo:rerun-if-changed=build.rs");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let ico_path = out_dir.join("nicotine.ico");
    generate_ico(&ico_path);

    match compile_res(&out_dir, &ico_path) {
        Ok(res_path) => {
            // Tell rustc to pass the .res file to the linker; the PE
            // linker picks up ICON resources automatically.
            println!("cargo:rustc-link-arg-bins={}", res_path.display());
        }
        Err(e) => {
            println!(
                "cargo:warning=Could not embed Windows .exe icon: {}. \
                 Install rc.exe (MSVC), llvm-rc, or windres to enable.",
                e
            );
        }
    }
}

/// Generate `nicotine.ico` with standard Windows icon sizes.
fn generate_ico(out: &Path) {
    let png_bytes = include_bytes!("assets/icon.png");
    let img = image::load_from_memory(png_bytes).expect("decode icon.png");

    let sizes: &[u32] = &[256, 128, 64, 48, 32, 16];
    let mut icon_dir = ico::IconDir::new(ico::ResourceType::Icon);
    for &size in sizes {
        let resized = img.resize_exact(size, size, image::imageops::FilterType::Lanczos3);
        let rgba = resized.to_rgba8();
        let image = ico::IconImage::from_rgba_data(size, size, rgba.into_raw());
        icon_dir.add_entry(ico::IconDirEntry::encode(&image).expect("encode ICO entry"));
    }

    let mut file = std::fs::File::create(out).expect("create ICO file");
    icon_dir.write(&mut file).expect("write ICO");
}

/// Compile a minimal .rc that references the icon into a .res. Returns
/// the path to the .res so the caller can hand it to the linker.
fn compile_res(out_dir: &Path, ico_path: &Path) -> Result<PathBuf, String> {
    let rc_path = out_dir.join("nicotine.rc");
    let res_path = out_dir.join("nicotine.res");

    // Resource compilers are picky about backslashes; escape them.
    let ico_rc_literal = ico_path.to_string_lossy().replace('\\', "\\\\");
    let rc_source = format!("1 ICON \"{}\"\n", ico_rc_literal);
    std::fs::write(&rc_path, rc_source).map_err(|e| format!("write .rc: {}", e))?;

    let tool =
        find_resource_compiler().ok_or_else(|| "no resource compiler found on PATH".to_string())?;

    // llvm-rc, rc.exe, and windres all accept different flag shapes.
    let mut cmd = Command::new(&tool);
    if tool.ends_with("windres") || tool.contains("windres") {
        cmd.args(["--input-format=rc", "--output-format=coff", "-i"])
            .arg(&rc_path)
            .arg("-o")
            .arg(&res_path);
    } else {
        // llvm-rc and rc.exe share the `/fo <out> <in>` flag form.
        cmd.arg("/fo").arg(&res_path).arg(&rc_path);
    }

    let output = cmd
        .output()
        .map_err(|e| format!("invoke {}: {}", tool, e))?;
    if !output.status.success() {
        return Err(format!(
            "{} exited with status {}: {}",
            tool,
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(res_path)
}

/// Return the first resource compiler we find on PATH. Order is chosen
/// so native Windows builds prefer `rc.exe`, cross-builds from Linux
/// pick up `llvm-rc` or the versioned `llvm-rc-NN` apt package.
fn find_resource_compiler() -> Option<String> {
    const CANDIDATES: &[&str] = &[
        "rc.exe",
        "llvm-rc",
        "llvm-rc-20",
        "llvm-rc-19",
        "llvm-rc-18",
        "llvm-rc-17",
        "llvm-rc-16",
        "llvm-rc-15",
        "windres",
        "x86_64-w64-mingw32-windres",
    ];
    for candidate in CANDIDATES {
        if which(candidate) {
            return Some((*candidate).to_string());
        }
    }
    None
}

fn which(name: &str) -> bool {
    if let Ok(path) = std::env::var("PATH") {
        let sep = if cfg!(windows) { ';' } else { ':' };
        for dir in path.split(sep) {
            let full = Path::new(dir).join(name);
            if full.is_file() {
                return true;
            }
            // Also try with .exe suffix on Windows hosts.
            if cfg!(windows) {
                let with_exe = full.with_extension("exe");
                if with_exe.is_file() {
                    return true;
                }
            }
        }
    }
    false
}

// SPDX-License-Identifier: AGPL-3.0-only
//! Embed the Quiver arrowhead icon into the Windows binary (ADR-0039).
//!
//! Checks CARGO_CFG_TARGET_OS (the *target*, not the host) so cross-compilation
//! from Linux for x86_64-pc-windows-gnu works correctly. On non-Windows targets
//! this is a no-op. winresource handles the cross-windres binary automatically.

fn main() {
    for size in [16u32, 32, 48, 64, 128, 256] {
        println!("cargo:rerun-if-changed=../../docs/assets/icon/quiver-{size}.png");
    }

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }

    let out_dir = match std::env::var("OUT_DIR") {
        Ok(d) => d,
        Err(_) => {
            println!("cargo:warning=OUT_DIR not set; skipping icon embedding");
            return;
        }
    };
    let ico_path = format!("{out_dir}/quiver.ico");

    let mut icon_dir = ico::IconDir::new(ico::ResourceType::Icon);
    for size in [16u32, 32, 48, 64, 128, 256] {
        let png_path = format!("../../docs/assets/icon/quiver-{size}.png");
        let png_data = match std::fs::read(&png_path) {
            Ok(d) => d,
            Err(e) => {
                println!("cargo:warning=icon {png_path} missing ({e}); run `just tui-shots`");
                continue;
            }
        };
        match ico::IconImage::read_png(std::io::Cursor::new(&png_data))
            .and_then(|img| ico::IconDirEntry::encode(&img))
        {
            Ok(entry) => icon_dir.add_entry(entry),
            Err(e) => println!("cargo:warning=could not encode {size}x{size} icon: {e}"),
        }
    }

    let mut f = match std::fs::File::create(&ico_path) {
        Ok(f) => f,
        Err(e) => {
            println!("cargo:warning=could not create {ico_path}: {e}");
            return;
        }
    };
    if let Err(e) = icon_dir.write(&mut f) {
        println!("cargo:warning=could not write ICO: {e}");
        return;
    }

    if let Err(e) = winresource::WindowsResource::new()
        .set_icon(&ico_path)
        .compile()
    {
        println!("cargo:warning=icon embedding failed: {e}");
    }
}

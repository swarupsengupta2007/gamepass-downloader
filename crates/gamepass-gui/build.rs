//! Embeds the app mark (`src/icon.rs`) as the exe's icon on Windows, so
//! Explorer, shortcuts and the taskbar show it before the window opens.

#[path = "src/icon.rs"]
mod icon;

fn main() {
    println!("cargo:rerun-if-changed=src/icon.rs");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let ico = out.join("app.ico");
    std::fs::write(&ico, ico_file(&[16, 20, 24, 32, 40, 48, 64, 96, 128, 256])).unwrap();
    let rc = out.join("app.rc");
    // ID 1 is the icon Explorer picks for the exe.
    std::fs::write(&rc, format!("1 ICON \"{}\"\n", ico.display().to_string().replace('\\', "\\\\"))).unwrap();
    embed_resource::compile(&rc, embed_resource::NONE).manifest_required().unwrap();
}

/// An .ico with one 32-bit BMP image per size.
fn ico_file(sizes: &[u32]) -> Vec<u8> {
    let images: Vec<Vec<u8>> = sizes.iter().map(|&side| bmp(side)).collect();
    let mut file = Vec::new();
    file.extend(0u16.to_le_bytes()); // reserved
    file.extend(1u16.to_le_bytes()); // icon
    file.extend((sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (&side, image) in sizes.iter().zip(&images) {
        let dimension = if side >= 256 { 0 } else { side as u8 }; // 0 means 256
        file.extend([dimension, dimension, 0, 0]); // width, height, no palette, reserved
        file.extend(1u16.to_le_bytes()); // planes
        file.extend(32u16.to_le_bytes()); // bits per pixel
        file.extend((image.len() as u32).to_le_bytes());
        file.extend(offset.to_le_bytes());
        offset += image.len() as u32;
    }
    for image in images {
        file.extend(image);
    }
    file
}

/// BITMAPINFOHEADER (height doubled for the mask), BGRA rows bottom-up, then
/// an all-clear AND mask: the alpha channel does the masking.
fn bmp(side: u32) -> Vec<u8> {
    let rgba = icon::rgba(side);
    let mask_row = side.div_ceil(32) * 4;
    let mut image = Vec::new();
    image.extend(40u32.to_le_bytes());
    image.extend((side as i32).to_le_bytes());
    image.extend((side as i32 * 2).to_le_bytes());
    image.extend(1u16.to_le_bytes());
    image.extend(32u16.to_le_bytes());
    image.extend([0u8; 24]); // no compression, sizes and palette left to the reader
    for y in (0..side).rev() {
        for x in 0..side {
            let i = ((y * side + x) * 4) as usize;
            image.extend([rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    image.extend(std::iter::repeat_n(0u8, (mask_row * side) as usize));
    image
}

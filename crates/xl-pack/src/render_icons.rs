//! Render the app icon SVG into the hicolor PNG sizes desktops expect.
//! `cargo run -p xl-pack --bin render-icons -- <icon.svg> <out-dir>`

use std::path::PathBuf;

use resvg::{tiny_skia, usvg};

const SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let svg = PathBuf::from(args.next().ok_or("usage: render-icons <icon.svg> <out-dir> [name]")?);
    let out = PathBuf::from(args.next().ok_or("missing <out-dir>")?);
    let name = args.next().unwrap_or_else(|| "dev.xl.Xl".into());
    let tree = usvg::Tree::from_data(&std::fs::read(&svg)?, &usvg::Options::default())?;
    for size in SIZES.iter().copied().chain([512]) {
        let mut pixmap = tiny_skia::Pixmap::new(size, size).ok_or("bad size")?;
        let scale = size as f32 / tree.size().width().max(tree.size().height());
        resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
        let dir = out.join(format!("{size}x{size}/apps"));
        std::fs::create_dir_all(&dir)?;
        pixmap.save_png(dir.join(format!("{name}.png")))?;
    }
    let scalable = out.join("scalable/apps");
    std::fs::create_dir_all(&scalable)?;
    std::fs::copy(&svg, scalable.join(format!("{name}.svg")))?;
    println!("rendered {name} into {}", out.display());
    Ok(())
}

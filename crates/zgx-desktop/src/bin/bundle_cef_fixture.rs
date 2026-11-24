#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use cef::build_util::mac::{BundleInfo, bundle};
    let output = std::env::args()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("target/cef-fixture"));
    let target = std::env::current_exe()?
        .parent()
        .ok_or("bundle executable has no directory")?
        .to_path_buf();
    let info = BundleInfo::new(
        "cef_web_view",
        "dev.zgx.cef-fixture",
        "CEF OSR fixture",
        "English",
        semver::Version::new(0, 1, 0),
    );
    let app = bundle(
        &output,
        &target,
        "cef_web_view",
        "cef_web_view_helper",
        None,
        info,
    )?;
    println!("CEF fixture bundled at {}", app.display());
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("This local fixture bundler currently supports macOS only.");
    std::process::exit(2);
}

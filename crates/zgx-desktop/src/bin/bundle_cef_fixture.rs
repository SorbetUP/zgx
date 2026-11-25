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
    // A fixed bundle identifier makes AppKit offer to restore old windows
    // after a previous fixture crash. That modal recovery prompt blocks the
    // offscreen event loop before the fixture can run. Keep each packaged test
    // app isolated from that persisted state; CEF data persistence is still
    // controlled separately through POLYSCALE_CEF_DATA_DIR.
    let bundle_id = std::env::var("ZGX_CEF_FIXTURE_BUNDLE_ID").unwrap_or_else(|_| {
        let run_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        format!("dev.zgx.cef-fixture.run-{run_id}")
    });
    let info = BundleInfo::new(
        "cef_web_view",
        &bundle_id,
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
    // CEF's framework and helper bundles carry nested signatures. Replacing
    // the executable while iterating can leave a stale ad-hoc signature; sign
    // the completed fixture as one bundle so macOS does not kill it with
    // CODESIGNING/Invalid Page before a smoke test starts.
    let sign = std::process::Command::new("codesign")
        .args(["--force", "--deep", "--sign", "-"])
        .arg(&app)
        .status()?;
    if !sign.success() {
        return Err("codesign could not ad-hoc sign the CEF fixture bundle".into());
    }
    let verify = std::process::Command::new("codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(&app)
        .status()?;
    if !verify.success() {
        return Err("codesign could not verify the CEF fixture bundle".into());
    }
    println!("CEF fixture bundled at {}", app.display());
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("This local fixture bundler currently supports macOS only.");
    std::process::exit(2);
}

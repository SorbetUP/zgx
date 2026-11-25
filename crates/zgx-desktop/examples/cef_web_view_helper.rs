//! Helper-process entry point used by the offscreen `cef_web_view` fixture.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Some(code) = zgx_desktop::execute_cef_helper_process()? {
        std::process::exit(code);
    }
    Ok(())
}

//! CEF helper process entry point used by `bundle-cef-app` for the OSR fixture.
fn main() -> std::process::ExitCode {
    match zgx_desktop::execute_cef_helper_process() {
        Ok(Some(code)) => std::process::ExitCode::from(code as u8),
        Ok(None) | Err(_) => std::process::ExitCode::FAILURE,
    }
}

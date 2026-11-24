//! CEF helper process entry point used by `bundle-cef-app` for the OSR fixture.
fn main() -> std::process::ExitCode {
    use cef::{args::Args, *};
    let args = Args::new();
    #[cfg(target_os = "macos")]
    let _sandbox = {
        let mut sandbox = cef::sandbox::Sandbox::new();
        sandbox.initialize(args.as_main_args());
        sandbox
    };
    #[cfg(target_os = "macos")]
    let _loader = {
        let Ok(exe) = std::env::current_exe() else {
            return std::process::ExitCode::FAILURE;
        };
        let loader = cef::library_loader::LibraryLoader::new(&exe, true);
        if !loader.load() {
            return std::process::ExitCode::FAILURE;
        }
        loader
    };
    let _ = cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);
    let code = cef::execute_process(
        Some(args.as_main_args()),
        None::<&mut App>,
        std::ptr::null_mut(),
    );
    if code < 0 {
        std::process::ExitCode::FAILURE
    } else {
        std::process::ExitCode::from(code as u8)
    }
}

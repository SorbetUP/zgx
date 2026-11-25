use std::{cell::RefCell, collections::HashMap, path::PathBuf, sync::Arc};

use cef::{self, args::Args, *};
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

pub(super) type PumpScheduler = Arc<dyn Fn(i64)>;

#[derive(Default)]
struct BrowserTracker {
    pending_creations: usize,
    open_ids: HashSet<i32>,
}

static BROWSERS: OnceLock<Mutex<BrowserTracker>> = OnceLock::new();

fn browsers() -> &'static Mutex<BrowserTracker> {
    BROWSERS.get_or_init(|| Mutex::new(BrowserTracker::default()))
}

thread_local! {
    static RUNTIME: RefCell<Option<Runtime>> = const { RefCell::new(None) };
    static SCHEDULER: RefCell<Option<PumpScheduler>> = const { RefCell::new(None) };
    static PROCESSED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    #[cfg(target_os = "windows")]
    static WINDOWS_MAIN_ARGS: RefCell<Option<cef::MainArgs>> = const { RefCell::new(None) };
    #[cfg(target_os = "windows")]
    static WINDOWS_SANDBOX_INFO: std::cell::Cell<*mut u8> = const { std::cell::Cell::new(std::ptr::null_mut()) };
    #[cfg(target_os = "macos")]
    static LIBRARY: RefCell<Option<cef::library_loader::LibraryLoader>> = const { RefCell::new(None) };
}

struct Runtime {
    contexts: HashMap<String, RequestContext>,
    browsers: HashMap<i32, Browser>,
    shutting_down: bool,
}

wrap_browser_process_handler! {
    struct PumpHandler { schedule: Arc<dyn Fn(i64)> }
    impl BrowserProcessHandler {
    fn on_schedule_message_pump_work(&self, delay_ms: i64) { (self.schedule)(delay_ms); }
    }
}

wrap_app! {
    struct CEFApplication { handler: BrowserProcessHandler, renderer: RenderProcessHandler, mock_keychain: bool }
    impl App {
        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> { Some(self.handler.clone()) }
        fn render_process_handler(&self) -> Option<RenderProcessHandler> { Some(self.renderer.clone()) }
        fn on_before_command_line_processing(
            &self,
            process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>,
        ) {
            #[cfg(target_os = "macos")]
            {
                let _ = process_type;
                if self.mock_keychain && let Some(command_line) = command_line {
                    let switch: CefString = "use-mock-keychain".into();
                    command_line.append_switch(Some(&switch));
                }
            }
            #[cfg(not(target_os = "macos"))]
            let _ = (process_type, command_line);
        }
    }
}

pub(super) fn set_scheduler(schedule: PumpScheduler) {
    SCHEDULER.with(|slot| *slot.borrow_mut() = Some(schedule));
}

pub(super) fn schedule_work(delay_ms: i64) {
    SCHEDULER.with(|slot| {
        if let Some(schedule) = slot.borrow().as_ref() {
            schedule(delay_ms);
        }
    });
}

/// Must run before ZGX or application data are initialized. A non-negative
/// result means CEF owns this process as a renderer/GPU/helper process.
pub(super) fn execute_process() -> Result<Option<i32>, String> {
    if PROCESSED.with(std::cell::Cell::get) {
        return Ok(None);
    }
    #[cfg(target_os = "windows")]
    if WINDOWS_MAIN_ARGS.with(|args| args.borrow().is_none()) {
        return Err("CEF on Windows must be launched through its sandbox bootstrap".into());
    }
    execute_process_with_args(Args::new(), std::ptr::null_mut())
}

#[cfg(target_os = "windows")]
pub(super) fn execute_process_with_windows_context(
    instance: cef::sys::HINSTANCE,
    sandbox_info: *mut u8,
) -> Result<Option<i32>, String> {
    if sandbox_info.is_null() {
        return Err("CEF Windows bootstrap did not provide sandbox information".into());
    }
    let main_args = cef::MainArgs { instance };
    WINDOWS_MAIN_ARGS.with(|args| *args.borrow_mut() = Some(main_args.clone()));
    WINDOWS_SANDBOX_INFO.with(|stored| stored.set(sandbox_info));
    execute_process_with_args(Args::from(main_args), sandbox_info)
}

fn execute_process_with_args(args: Args, sandbox_info: *mut u8) -> Result<Option<i32>, String> {
    if PROCESSED.with(std::cell::Cell::get) {
        return Ok(None);
    }
    #[cfg(target_os = "macos")]
    LIBRARY.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            let exe = std::env::current_exe().map_err(|error| error.to_string())?;
            let loader = cef::library_loader::LibraryLoader::new(&exe, false);
            if !loader.load() {
                return Err("CEF framework failed to load".to_owned());
            }
            *slot = Some(loader);
        }
        Ok::<(), String>(())
    })?;
    let _ = cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);
    let handler = PumpHandler::new(Arc::new(|_| {}));
    let renderer = super::bridge::render_process_handler();
    let mut app = CEFApplication::new(handler, renderer, use_mock_keychain());
    let code = cef::execute_process(Some(args.as_main_args()), Some(&mut app), sandbox_info);
    PROCESSED.with(|done| done.set(true));
    Ok((code >= 0).then_some(code))
}

/// Run the bundled helper executable with the same renderer-process
/// integration as the browser process. CEF requires the App to be supplied to
/// `execute_process` in renderers; passing `None` silently disables the V8
/// message bridge.
pub(super) fn execute_helper_process() -> Result<Option<i32>, String> {
    let args = Args::new();
    #[cfg(target_os = "macos")]
    let _sandbox = {
        let mut sandbox = cef::sandbox::Sandbox::new();
        sandbox.initialize(args.as_main_args());
        sandbox
    };
    #[cfg(target_os = "macos")]
    LIBRARY.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            let exe = std::env::current_exe().map_err(|error| error.to_string())?;
            let loader = cef::library_loader::LibraryLoader::new(&exe, true);
            if !loader.load() {
                return Err("CEF framework failed to load in helper".to_owned());
            }
            *slot = Some(loader);
        }
        Ok::<(), String>(())
    })?;
    let _ = cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);
    let handler = PumpHandler::new(Arc::new(|_| {}));
    let renderer = super::bridge::render_process_handler();
    let mut app = CEFApplication::new(handler, renderer, use_mock_keychain());
    let code = cef::execute_process(
        Some(args.as_main_args()),
        Some(&mut app),
        std::ptr::null_mut(),
    );
    Ok((code >= 0).then_some(code))
}

fn runtime_dir() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("POLYSCALE_CEF_DIR") {
        return Ok(PathBuf::from(path));
    }
    std::env::current_exe()
        .map_err(|error| format!("cannot locate executable: {error}"))?
        .parent()
        .map(PathBuf::from)
        .ok_or_else(|| "executable has no parent directory".into())
}

fn initialize_inner() -> Result<Runtime, String> {
    if !PROCESSED.with(std::cell::Cell::get) {
        return Err("CEF execute_process() must run before ZGX application initialization".into());
    }
    #[cfg(target_os = "windows")]
    let args = Args::from(
        WINDOWS_MAIN_ARGS
            .with(|args| args.borrow().clone())
            .ok_or("CEF Windows main args were not provided by the sandbox bootstrap")?,
    );
    #[cfg(not(target_os = "windows"))]
    let args = Args::new();
    let root = runtime_dir()?;
    let app_name = std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "Polyscale".into());
    #[cfg(not(target_os = "windows"))]
    let helper_name =
        std::env::var("POLYSCALE_CEF_HELPER_APP").unwrap_or_else(|_| format!("{app_name} Helper"));
    #[cfg(target_os = "macos")]
    let framework = root.join("../Frameworks/Chromium Embedded Framework.framework");
    #[cfg(target_os = "macos")]
    let helper = root.join(format!(
        "../Frameworks/{helper_name}.app/Contents/MacOS/{helper_name}"
    ));
    #[cfg(target_os = "macos")]
    let resources = framework.join("Resources");
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let helper = root.join(if cfg!(target_os = "windows") {
        format!("{helper_name}.exe")
    } else {
        helper_name
    });
    #[cfg(not(target_os = "macos"))]
    let resources = root.clone();
    let cache_root = data_root()?;
    std::fs::create_dir_all(&cache_root)
        .map_err(|error| format!("cannot create CEF cache root: {error}"))?;
    let log_file = std::env::var_os("POLYSCALE_CEF_LOG").map(PathBuf::from);
    let settings = Settings {
        windowless_rendering_enabled: 1,
        external_message_pump: 1,
        no_sandbox: 0,
        #[cfg(not(target_os = "windows"))]
        browser_subprocess_path: helper.to_string_lossy().as_ref().into(),
        #[cfg(target_os = "macos")]
        framework_dir_path: framework.to_string_lossy().as_ref().into(),
        resources_dir_path: resources.to_string_lossy().as_ref().into(),
        locales_dir_path: resources.join("locales").to_string_lossy().as_ref().into(),
        root_cache_path: cache_root.to_string_lossy().as_ref().into(),
        log_file: log_file
            .as_ref()
            .map(|path| path.to_string_lossy().as_ref().into())
            .unwrap_or_default(),
        log_severity: LogSeverity::WARNING,
        ..Default::default()
    };
    let schedule = SCHEDULER
        .with(|slot| slot.borrow().clone())
        .ok_or("CEF message pump was not connected to the ZGX event loop")?;
    let handler = PumpHandler::new(schedule);
    let mut app = CEFApplication::new(
        handler,
        super::bridge::render_process_handler(),
        use_mock_keychain(),
    );
    #[cfg(target_os = "windows")]
    let sandbox_info = WINDOWS_SANDBOX_INFO.with(std::cell::Cell::get);
    #[cfg(not(target_os = "windows"))]
    let sandbox_info = std::ptr::null_mut();
    let initialized = cef::initialize(
        Some(args.as_main_args()),
        Some(&settings),
        Some(&mut app),
        sandbox_info,
    );
    if initialized == 0 {
        return Err("CEF initialization failed (sandbox is required)".into());
    }
    Ok(Runtime {
        contexts: HashMap::new(),
        browsers: HashMap::new(),
        shutting_down: false,
    })
}

fn use_mock_keychain() -> bool {
    mock_keychain_requested(|name| std::env::var_os(name).is_some())
}

fn mock_keychain_requested(mut present: impl FnMut(&str) -> bool) -> bool {
    [
        "ZGX_CEF_TEST_FIXTURE",
        "ZGX_CEF_PROFILE_FIXTURE",
        "ZGX_CEF_EVAL_RESULT",
        "ZGX_CEF_EVENT_RESULT",
        "ZGX_CEF_DOWNLOAD_RESULT",
        "ZGX_CEF_INPUT_RESULT",
        "ZGX_CEF_SITE_DATA_RESULT",
        "ZGX_CEF_LATE_LOAD_RESULT",
    ]
    .iter()
    .any(|name| present(name))
}

/// Initialize CEF once on the UI thread. The caller must run
/// `execute_process()` before constructing the ZGX app to handle helper
/// processes without starting SQLite, Luau or a window.
pub(super) fn initialize() -> Result<(), String> {
    RUNTIME.with(|runtime| {
        if runtime.borrow().is_some() {
            return Ok(());
        }
        *runtime.borrow_mut() = Some(initialize_inner()?);
        Ok(())
    })
}

pub(super) fn request_context(profile: &str, persistent: bool) -> Result<RequestContext, String> {
    initialize()?;
    RUNTIME.with(|runtime| {
        let mut runtime = runtime.borrow_mut();
        let runtime = runtime.as_mut().expect("runtime initialized");
        if let Some(context) = runtime.contexts.get(profile) {
            return Ok(context.clone());
        }
        let mut settings = RequestContextSettings::default();
        if persistent {
            let root = data_root()?;
            std::fs::create_dir_all(&root)
                .map_err(|error| format!("cannot create CEF profile store: {error}"))?;
            let cache_path = root.join(profile_component(profile)?);
            std::fs::create_dir_all(&cache_path)
                .map_err(|error| format!("cannot create CEF profile store: {error}"))?;
            settings.cache_path = cache_path.to_string_lossy().as_ref().into();
        }
        let context =
            cef::request_context_create_context(Some(&settings), None).ok_or_else(|| {
                format!("CEF could not create request context for profile `{profile}`")
            })?;
        runtime.contexts.insert(profile.to_owned(), context.clone());
        Ok(context)
    })
}

fn data_root() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("POLYSCALE_CEF_DATA_DIR") {
        return Ok(PathBuf::from(path));
    }
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOME is not set")?
        .join("Library/Application Support/Polyscale/Chromium");
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or("LOCALAPPDATA is not set")?
        .join("Polyscale/Chromium");
    #[cfg(target_os = "linux")]
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .ok_or("no user data directory is set")?
        .join("polyscale/chromium");
    Ok(base)
}

fn profile_component(profile: &str) -> Result<String, String> {
    if profile.is_empty() || profile.len() > 512 {
        return Err("CEF profile ID must contain 1 to 512 bytes".into());
    }
    let mut encoded = String::from("p-");
    for byte in profile.bytes() {
        encoded.push_str(&format!("{byte:02x}"));
    }
    Ok(encoded)
}

pub(super) fn pump() {
    RUNTIME.with(|runtime| {
        if runtime.borrow().is_some() {
            cef::do_message_loop_work();
        }
    });
}

pub(super) fn browser_creation_started() {
    browsers()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .pending_creations += 1;
}

pub(super) fn browser_creation_failed() {
    let mut tracker = browsers()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    tracker.pending_creations = tracker
        .pending_creations
        .checked_sub(1)
        .expect("CEF creation failure without a pending browser creation");
}

pub(super) fn browser_created(identifier: i32) {
    let mut tracker = browsers()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    tracker.pending_creations = tracker
        .pending_creations
        .checked_sub(1)
        .expect("CEF created a browser without a pending creation");
    assert!(
        tracker.open_ids.insert(identifier),
        "CEF reused a browser ID while the previous browser was still open"
    );
}

pub(super) fn register_browser(browser: &Browser) -> Result<(), String> {
    let identifier = browser.identifier();
    let mut failure = None;
    RUNTIME.with(|runtime| {
        let mut runtime = runtime.borrow_mut();
        let Some(runtime) = runtime.as_mut() else {
            failure = Some("CEF browser arrived after runtime shutdown".to_owned());
            return;
        };
        if runtime.shutting_down {
            failure = Some("CEF is shutting down".to_owned());
            return;
        }
        runtime.browsers.insert(identifier, browser.clone());
    });
    failure.map_or(Ok(()), Err)
}

pub(super) fn is_shutting_down() -> bool {
    RUNTIME.with(|runtime| {
        runtime
            .borrow()
            .as_ref()
            .is_none_or(|runtime| runtime.shutting_down)
    })
}

pub(super) fn begin_shutdown() {
    let open = RUNTIME.with(|runtime| {
        let mut runtime = runtime.borrow_mut();
        let Some(runtime) = runtime.as_mut() else {
            return Vec::new();
        };
        runtime.shutting_down = true;
        runtime.browsers.values().cloned().collect::<Vec<_>>()
    });
    for browser in open {
        if let Some(host) = browser.host() {
            host.close_browser(1);
        }
    }
}

pub(super) fn browser_closed(identifier: i32) {
    let removed = browsers()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .open_ids
        .remove(&identifier);
    assert!(
        removed,
        "CEF closed an unknown or already-closed browser ID"
    );
    RUNTIME.with(|runtime| {
        if let Some(runtime) = runtime.borrow_mut().as_mut() {
            runtime.browsers.remove(&identifier);
        }
    });
    schedule_work(0);
}

pub(super) fn has_browsers() -> bool {
    let tracker = browsers()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    tracker.pending_creations > 0 || !tracker.open_ids.is_empty()
}

pub(super) fn ensure_accepting_browsers() -> Result<(), String> {
    if is_shutting_down() {
        Err("CEF is shutting down and cannot create another browser".into())
    } else {
        Ok(())
    }
}

pub(super) fn shutdown() -> Result<(), String> {
    if has_browsers() {
        return Err("CEF shutdown requested while browser creation or closure is pending".into());
    }
    RUNTIME.with(|runtime| {
        if runtime.borrow_mut().take().is_some() {
            cef::shutdown();
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        browser_closed, browser_created, browser_creation_failed, browser_creation_started,
        has_browsers, mock_keychain_requested,
    };

    #[test]
    fn every_automated_fixture_uses_the_mock_keychain() {
        for fixture in [
            "ZGX_CEF_TEST_FIXTURE",
            "ZGX_CEF_PROFILE_FIXTURE",
            "ZGX_CEF_EVAL_RESULT",
            "ZGX_CEF_EVENT_RESULT",
            "ZGX_CEF_DOWNLOAD_RESULT",
            "ZGX_CEF_INPUT_RESULT",
            "ZGX_CEF_SITE_DATA_RESULT",
            "ZGX_CEF_LATE_LOAD_RESULT",
        ] {
            assert!(mock_keychain_requested(|name| name == fixture), "{fixture}");
        }
        assert!(!mock_keychain_requested(|_| false));
    }

    #[test]
    fn tracks_pending_creations_and_actual_browser_ids() {
        assert!(!has_browsers());
        browser_creation_started();
        assert!(
            has_browsers(),
            "an accepted creation must keep shutdown alive"
        );

        browser_created(9_314_159);
        assert!(has_browsers(), "the actual CEF ID must stay tracked");
        assert!(
            super::shutdown().is_err(),
            "shutdown must wait for on_before_close"
        );
        browser_closed(9_314_159);
        assert!(!has_browsers(), "only on_before_close releases the browser");

        browser_creation_started();
        browser_creation_failed();
        assert!(
            !has_browsers(),
            "rejected creation must release its pending slot"
        );
    }
}

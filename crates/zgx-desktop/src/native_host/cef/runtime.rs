use std::{cell::RefCell, collections::HashMap, path::PathBuf, sync::Arc};

use cef::{self, args::Args, *};
use std::sync::atomic::{AtomicUsize, Ordering};

static BROWSERS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static RUNTIME: RefCell<Option<Runtime>> = const { RefCell::new(None) };
    static SCHEDULER: RefCell<Option<Arc<dyn Fn(i64)>>> = const { RefCell::new(None) };
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
}

wrap_browser_process_handler! {
    struct PumpHandler { schedule: Arc<dyn Fn(i64)> }
    impl BrowserProcessHandler {
    fn on_schedule_message_pump_work(&self, delay_ms: i64) { (self.schedule)(delay_ms); }
    }
}

wrap_app! {
    struct CEFApplication { handler: BrowserProcessHandler }
    impl App { fn browser_process_handler(&self) -> Option<BrowserProcessHandler> { Some(self.handler.clone()) } }
}

pub(super) fn set_scheduler(schedule: Arc<dyn Fn(i64)>) {
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
    let code = cef::execute_process(Some(args.as_main_args()), None::<&mut App>, sandbox_info);
    PROCESSED.with(|done| done.set(true));
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
    let mut app = CEFApplication::new(handler);
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
    })
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

pub(super) fn request_context(profile: &str) -> Result<RequestContext, String> {
    initialize()?;
    RUNTIME.with(|runtime| {
        let mut runtime = runtime.borrow_mut();
        let runtime = runtime.as_mut().expect("runtime initialized");
        if let Some(context) = runtime.contexts.get(profile) {
            return Ok(context.clone());
        }
        let root = data_root()?;
        std::fs::create_dir_all(&root)
            .map_err(|error| format!("cannot create CEF profile store: {error}"))?;
        let cache_path = root.join(profile_component(profile)?);
        std::fs::create_dir_all(&cache_path)
            .map_err(|error| format!("cannot create CEF profile store: {error}"))?;
        let mut settings = RequestContextSettings::default();
        settings.cache_path = cache_path.to_string_lossy().as_ref().into();
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

pub(super) fn browser_opened() {
    BROWSERS.fetch_add(1, Ordering::AcqRel);
}
pub(super) fn browser_closed() {
    let _ = BROWSERS.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
        Some(n.saturating_sub(1))
    });
}
pub(super) fn has_browsers() -> bool {
    BROWSERS.load(Ordering::Acquire) != 0
}

pub(super) fn shutdown() {
    RUNTIME.with(|runtime| {
        if runtime.borrow_mut().take().is_some() {
            debug_assert!(
                !has_browsers(),
                "CEF shutdown requested before browser callbacks closed"
            );
            cef::shutdown();
        }
    });
}

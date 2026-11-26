#[cfg(debug_assertions)]
use std::io::Write;
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    path::PathBuf,
    rc::Rc,
    sync::Arc,
};

use cef::{self, args::Args, *};
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock, atomic::AtomicBool};

pub(super) type PumpScheduler = Arc<dyn Fn(i64)>;

#[derive(Default)]
struct BrowserTracker {
    next_creation_id: u64,
    // The bool is true after the owning page was canceled. Keep canceled,
    // accepted creations in this set until CEF confirms creation or failure:
    // shutdown must not race a late `on_after_created` callback.
    pending_creations: HashMap<u64, bool>,
    open_ids: HashSet<i32>,
}

static BROWSERS: OnceLock<Mutex<BrowserTracker>> = OnceLock::new();
#[cfg(debug_assertions)]
static CLOSE_RACE_DIAGNOSTIC_LOCK: Mutex<()> = Mutex::new(());

#[cfg(debug_assertions)]
pub(super) fn trace_close_race_impl(message: &str) {
    let result = std::env::var_os("ZGX_CEF_STRESS_RESULT")
        .or_else(|| std::env::var_os("ZGX_CEF_CLOSE_RACE_RESULT"))
        .or_else(|| std::env::var_os("ZGX_CEF_DRAG_RESULT"))
        .or_else(|| std::env::var_os("ZGX_CEF_INPUT_RESULT"));
    let Some(path) = result.map(PathBuf::from) else {
        return;
    };
    let executable = std::env::current_exe().ok().and_then(|path| {
        path.file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
    });
    let data_dir = std::env::var_os("POLYSCALE_CEF_DATA_DIR").map(PathBuf::from);
    if !close_race_trace_allowed(
        executable.as_deref(),
        data_dir.as_deref(),
        path.parent(),
        &std::env::temp_dir(),
        std::env::var_os("ZGX_CEF_CLOSE_RACE_RESULT").is_some()
            || std::env::var_os("ZGX_CEF_STRESS_RESULT").is_some()
            || std::env::var_os("ZGX_CEF_DRAG_RESULT").is_some()
            || std::env::var_os("ZGX_CEF_INPUT_RESULT").is_some(),
    ) {
        return;
    }
    let _guard = CLOSE_RACE_DIAGNOSTIC_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let diagnostic = path.with_extension("diagnostic");
    // The 100-cycle stress fixture emits a small lifecycle record for every
    // browser. Keep the log bounded, but large enough to retain the full run.
    if std::fs::metadata(&diagnostic).is_ok_and(|metadata| metadata.len() >= 1024 * 1024) {
        return;
    }
    let message: String = message.chars().take(240).collect();
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(diagnostic)
        .and_then(|mut file| writeln!(file, "{message}"));
}

#[cfg(debug_assertions)]
pub(super) fn close_race_fixture_enabled() -> bool {
    let Some(path) = std::env::var_os("ZGX_CEF_CLOSE_RACE_RESULT").map(PathBuf::from) else {
        return false;
    };
    let executable = std::env::current_exe().ok().and_then(|path| {
        path.file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
    });
    let data_dir = std::env::var_os("POLYSCALE_CEF_DATA_DIR").map(PathBuf::from);
    close_race_trace_allowed(
        executable.as_deref(),
        data_dir.as_deref(),
        path.parent(),
        &std::env::temp_dir(),
        true,
    )
}

#[cfg(not(debug_assertions))]
#[inline(always)]
pub(super) fn close_race_fixture_enabled() -> bool {
    false
}

#[cfg(debug_assertions)]
fn close_race_trace_allowed(
    executable: Option<&str>,
    data_dir: Option<&std::path::Path>,
    diagnostic_parent: Option<&std::path::Path>,
    temp_dir: &std::path::Path,
    fixture_flag: bool,
) -> bool {
    if !fixture_flag
        || executable != Some("cef_web_view")
        || !mock_keychain_allowed(executable, data_dir, temp_dir, true)
    {
        return false;
    }
    let (Some(parent), Ok(temp_dir)) = (diagnostic_parent, temp_dir.canonicalize()) else {
        return false;
    };
    parent
        .canonicalize()
        .is_ok_and(|parent| parent.starts_with(temp_dir))
}

fn browsers() -> &'static Mutex<BrowserTracker> {
    BROWSERS.get_or_init(|| Mutex::new(BrowserTracker::default()))
}

thread_local! {
    static RUNTIME: RefCell<Option<Runtime>> = const { RefCell::new(None) };
    static SCHEDULER: RefCell<Option<PumpScheduler>> = const { RefCell::new(None) };
    static PUMPING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static PUMP_DEFERRED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    #[cfg(debug_assertions)]
    static PUMP_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PROCESSED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    #[cfg(target_os = "windows")]
    static WINDOWS_MAIN_ARGS: RefCell<Option<cef::MainArgs>> = const { RefCell::new(None) };
    #[cfg(target_os = "windows")]
    static WINDOWS_SANDBOX_INFO: std::cell::Cell<*mut u8> = const { std::cell::Cell::new(std::ptr::null_mut()) };
    #[cfg(target_os = "macos")]
    static LIBRARY: RefCell<Option<cef::library_loader::LibraryLoader>> = const { RefCell::new(None) };
}
static SHUTDOWN_COMPLETE: AtomicBool = AtomicBool::new(false);

struct Runtime {
    contexts: HashMap<String, RequestContext>,
    permissions: HashMap<String, PermissionStoreHandle>,
    browsers: HashMap<i32, Browser>,
    deferred_browser_closes: VecDeque<Browser>,
    shutting_down: bool,
}

pub(super) type PermissionStoreHandle = Rc<RefCell<SitePermissionStore>>;

pub(super) struct SitePermissionStore {
    decisions: HashMap<String, bool>,
    path: Option<PathBuf>,
}

impl SitePermissionStore {
    pub(super) fn decision(&self, origin: &str, permission: &str) -> Option<bool> {
        self.decisions
            .get(&permission_key(origin, permission))
            .copied()
    }

    pub(super) fn remember(
        &mut self,
        origin: &str,
        permission: &str,
        allowed: bool,
    ) -> Result<(), String> {
        self.decisions
            .insert(permission_key(origin, permission), allowed);
        self.persist()
    }

    pub(super) fn forget_origin(&mut self, origin: &str) -> Result<(), String> {
        let prefix = format!("{origin}\n");
        self.decisions.retain(|key, _| !key.starts_with(&prefix));
        self.persist()
    }

    fn persist(&self) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let bytes = serde_json::to_vec(&self.decisions)
            .map_err(|error| format!("cannot encode website permissions: {error}"))?;
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, bytes)
            .map_err(|error| format!("cannot write website permissions: {error}"))?;
        #[cfg(target_os = "windows")]
        if path.exists() {
            std::fs::remove_file(path)
                .map_err(|error| format!("cannot replace website permissions: {error}"))?;
        }
        std::fs::rename(&temporary, path)
            .map_err(|error| format!("cannot save website permissions: {error}"))
    }
}

fn permission_key(origin: &str, permission: &str) -> String {
    format!("{origin}\n{permission}")
}

wrap_browser_process_handler! {
    struct PumpHandler { schedule: Arc<dyn Fn(i64)> }
    impl BrowserProcessHandler {
    fn on_schedule_message_pump_work(&self, delay_ms: i64) {
        schedule_while_running(
            SHUTDOWN_COMPLETE.load(std::sync::atomic::Ordering::Acquire),
            Some(&self.schedule),
            delay_ms,
        );
    }
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
    if !SHUTDOWN_COMPLETE.load(std::sync::atomic::Ordering::Acquire) {
        SCHEDULER.with(|slot| *slot.borrow_mut() = Some(schedule));
    }
}

pub(super) fn schedule_work(delay_ms: i64) {
    SCHEDULER.with(|slot| {
        schedule_while_running(
            SHUTDOWN_COMPLETE.load(std::sync::atomic::Ordering::Acquire),
            slot.borrow().as_ref(),
            delay_ms,
        );
    });
}

fn schedule_while_running(
    shutdown_complete: bool,
    scheduler: Option<&PumpScheduler>,
    delay_ms: i64,
) -> bool {
    if shutdown_complete {
        return false;
    }
    let Some(scheduler) = scheduler else {
        return false;
    };
    scheduler(delay_ms);
    true
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
        permissions: HashMap::new(),
        browsers: HashMap::new(),
        deferred_browser_closes: VecDeque::new(),
        shutting_down: false,
    })
}

fn use_mock_keychain() -> bool {
    let executable = std::env::current_exe().ok().and_then(|path| {
        path.file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
    });
    let data_dir = std::env::var_os("POLYSCALE_CEF_DATA_DIR").map(PathBuf::from);
    mock_keychain_allowed(
        executable.as_deref(),
        data_dir.as_deref(),
        &std::env::temp_dir(),
        mock_keychain_requested(|name| std::env::var_os(name).is_some()),
    )
}

fn mock_keychain_allowed(
    executable: Option<&str>,
    data_dir: Option<&std::path::Path>,
    temp_dir: &std::path::Path,
    fixture_flag: bool,
) -> bool {
    if !fixture_flag || !matches!(executable, Some("cef_web_view" | "browser_profile_fixture")) {
        return false;
    }
    let (Some(data_dir), Ok(temp_dir)) = (data_dir, temp_dir.canonicalize()) else {
        return false;
    };
    data_dir
        .canonicalize()
        .is_ok_and(|data_dir| data_dir.starts_with(temp_dir))
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
        "ZGX_CEF_CLOSE_RACE_RESULT",
        "ZGX_CEF_STRESS_RESULT",
        "ZGX_CEF_NAV_EVAL_RACE_RESULT",
        "ZGX_CEF_POPUP_RESULT",
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
        if persistent {
            if let Some(context) = runtime.contexts.get(profile) {
                return Ok(context.clone());
            }
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
        if persistent {
            runtime.contexts.insert(profile.to_owned(), context.clone());
        }
        Ok(context)
    })
}

pub(super) fn permission_store(
    profile: &str,
    persistent: bool,
) -> Result<PermissionStoreHandle, String> {
    initialize()?;
    RUNTIME.with(|runtime| {
        let mut runtime = runtime.borrow_mut();
        let runtime = runtime.as_mut().expect("runtime initialized");
        if persistent {
            if let Some(store) = runtime.permissions.get(profile) {
                return Ok(store.clone());
            }
        }
        let path = if persistent {
            let directory = data_root()?.join(profile_component(profile)?);
            std::fs::create_dir_all(&directory)
                .map_err(|error| format!("cannot create website permission store: {error}"))?;
            Some(directory.join("site-permissions.json"))
        } else {
            None
        };
        let decisions = match path.as_ref().filter(|path| path.is_file()) {
            Some(path) => {
                let bytes = std::fs::read(path)
                    .map_err(|error| format!("cannot read website permissions: {error}"))?;
                serde_json::from_slice(&bytes)
                    .map_err(|error| format!("invalid website permission store: {error}"))?
            }
            _ => HashMap::new(),
        };
        let store = Rc::new(RefCell::new(SitePermissionStore { decisions, path }));
        if persistent {
            runtime
                .permissions
                .insert(profile.to_owned(), store.clone());
        }
        Ok(store)
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
    #[cfg(debug_assertions)]
    let pump_number = PUMP_CALLS.with(|count| {
        let next = count.get().saturating_add(1);
        count.set(next);
        next
    });
    #[cfg(debug_assertions)]
    if pump_number <= 32 {
        super::cef_trace!(&format!("PUMP begin n={pump_number}"));
    }
    if !PUMPING.with(enter_pump) {
        super::cef_trace!("PUMP reentrant deferred");
        // CEF may enter a nested native message loop while work is running.
        // Record the request; do not schedule into the nested loop, which
        // could otherwise spin before the active call returns.
        PUMP_DEFERRED.with(defer_pump);
        return;
    }
    RUNTIME.with(|runtime| {
        if runtime.borrow().is_some() {
            cef::do_message_loop_work();
        }
    });
    PUMPING.with(|pumping| pumping.set(false));
    if PUMP_DEFERRED.with(take_deferred_pump) {
        super::cef_trace!("PUMP deferred request scheduled after return");
        schedule_work(0);
    }
}

fn enter_pump(pumping: &std::cell::Cell<bool>) -> bool {
    if pumping.get() {
        false
    } else {
        pumping.set(true);
        true
    }
}

fn defer_pump(deferred: &std::cell::Cell<bool>) {
    deferred.set(true);
}

fn take_deferred_pump(deferred: &std::cell::Cell<bool>) -> bool {
    deferred.replace(false)
}

pub(super) fn browser_creation_started() -> u64 {
    let mut tracker = browsers()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    tracker.next_creation_id = tracker
        .next_creation_id
        .checked_add(1)
        .expect("CEF browser creation ID exhausted");
    let id = tracker.next_creation_id;
    assert!(
        tracker.pending_creations.insert(id, false).is_none(),
        "CEF browser creation ID was reused"
    );
    super::cef_trace!(&format!("CREATE_START id={id}"));
    id
}

pub(super) fn browser_creation_failed(creation_id: u64) -> bool {
    let mut tracker = browsers()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let removed = tracker.pending_creations.remove(&creation_id).is_some();
    super::cef_trace!(&format!(
        "CREATE_REJECTED id={creation_id} tracked={removed}"
    ));
    removed
}

/// Marks an accepted asynchronous creation as unwanted without releasing it
/// from the shutdown barrier. CEF may still complete the request; its callback
/// must then close the browser and keep it tracked until `on_before_close`.
pub(super) fn browser_creation_cancelled(creation_id: u64) -> bool {
    let mut tracker = browsers()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let Some(cancelled) = tracker.pending_creations.get_mut(&creation_id) else {
        return false;
    };
    if *cancelled {
        super::cef_trace!(&format!("CREATE_CANCEL_DUPLICATE id={creation_id}"));
        return false;
    }
    *cancelled = true;
    super::cef_trace!(&format!("CREATE_CANCEL id={creation_id}"));
    true
}

/// Records every browser CEF actually creates, even if its creation token was
/// already canceled. The return value tells the caller whether this browser
/// still belongs to a live creation request; late browsers must be closed.
pub(super) fn browser_created(creation_id: u64, identifier: i32) -> bool {
    let mut tracker = browsers()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let creation_was_pending = tracker.pending_creations.remove(&creation_id) == Some(false);
    super::cef_trace!(&format!(
        "ON_AFTER_CREATED id={creation_id} browser={identifier} active={creation_was_pending}"
    ));
    assert!(
        tracker.open_ids.insert(identifier),
        "CEF reused a browser ID while the previous browser was still open"
    );
    creation_was_pending
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
        if runtime.browsers.contains_key(&identifier) {
            failure = Some(format!(
                "CEF reused browser identifier {identifier} while it is still tracked"
            ));
            return;
        }
        if runtime.shutting_down {
            failure = Some("CEF is shutting down".to_owned());
            return;
        }
        runtime.browsers.insert(identifier, browser.clone());
    });
    failure.map_or(Ok(()), Err)
}

/// Queue a browser close requested from `on_after_created` or another CEF
/// callback. CEF 156 on macOS can re-enter browser teardown from
/// `CloseBrowser` while `do_message_loop_work` is still creating the browser;
/// keep the real browser tracked, then close it on the next ZGX event-loop
/// turn, after the active CEF pump has returned.
pub(super) fn defer_browser_close(browser: Browser, _reason: &str) {
    let identifier = browser.identifier();
    let queued = RUNTIME.with(|runtime| {
        let mut runtime = runtime.borrow_mut();
        let Some(runtime) = runtime.as_mut() else {
            return false;
        };
        runtime
            .browsers
            .entry(identifier)
            .or_insert_with(|| browser.clone());
        runtime.deferred_browser_closes.push_back(browser);
        true
    });
    #[cfg(debug_assertions)]
    super::cef_trace!(&format!(
        "CLOSE_REQUEST_DEFERRED reason={_reason} browser={identifier} queued={queued}"
    ));
    if queued {
        schedule_work(0);
    }
}

/// Execute callback-originated close requests only from a later event-loop
/// turn. Call before entering the next CEF pump, never from a CEF callback.
pub(super) fn close_deferred_browsers() {
    if PUMPING.with(std::cell::Cell::get) {
        super::cef_trace!("CLOSE_REQUEST_DISPATCH_DEFERRED_ACTIVE_PUMP");
        PUMP_DEFERRED.with(defer_pump);
        return;
    }
    let pending = RUNTIME.with(|runtime| {
        let mut runtime = runtime.borrow_mut();
        runtime
            .as_mut()
            .map(|runtime| std::mem::take(&mut runtime.deferred_browser_closes))
            .unwrap_or_default()
    });
    for browser in pending {
        #[cfg(debug_assertions)]
        let identifier = browser.identifier();
        #[cfg(debug_assertions)]
        super::cef_trace!(&format!("CLOSE_REQUEST_DISPATCHED browser={identifier}"));
        if let Some(host) = browser.host() {
            host.close_browser(1);
        } else {
            #[cfg(debug_assertions)]
            super::cef_trace!(&format!("CLOSE_REQUEST_NO_HOST browser={identifier}"));
        }
    }
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
    #[cfg(debug_assertions)]
    let (pending, browsers) = {
        let tracker = browsers()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        (tracker.pending_creations.len(), tracker.open_ids.len())
    };
    #[cfg(debug_assertions)]
    super::cef_trace!(&format!("SHUTDOWN_BEGIN pending={pending} open={browsers}"));
    for browser in open {
        if let Some(host) = browser.host() {
            #[cfg(debug_assertions)]
            super::cef_trace!(&format!(
                "SHUTDOWN_CLOSE_REQUEST browser={}",
                browser.identifier()
            ));
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
    super::cef_trace!(&format!("ON_BEFORE_CLOSE browser={identifier}"));
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
    !tracker.pending_creations.is_empty() || !tracker.open_ids.is_empty()
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
        super::cef_trace!("SHUTDOWN_BLOCKED browser_or_creation_pending");
        return Err("CEF shutdown requested while browser creation or closure is pending".into());
    }
    super::cef_trace!("CEF_SHUTDOWN");
    SHUTDOWN_COMPLETE.store(true, std::sync::atomic::Ordering::Release);
    SCHEDULER.with(|scheduler| {
        scheduler.borrow_mut().take();
    });
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
        browser_closed, browser_created, browser_creation_cancelled, browser_creation_failed,
        browser_creation_started, close_race_trace_allowed, has_browsers, mock_keychain_allowed,
        mock_keychain_requested, schedule_while_running,
    };
    use std::{collections::HashMap, fs, path::Path, time::SystemTime};

    #[test]
    fn a_reentrant_cef_pump_is_deferred_until_the_active_call_returns() {
        let pumping = std::cell::Cell::new(false);
        let deferred = std::cell::Cell::new(false);
        let calls = std::cell::Cell::new(0);
        assert!(super::enter_pump(&pumping));
        calls.set(calls.get() + 1);
        if super::enter_pump(&pumping) {
            calls.set(calls.get() + 1);
        } else {
            super::defer_pump(&deferred);
        }
        assert_eq!(calls.get(), 1, "a nested event cannot call CEF immediately");
        assert!(super::take_deferred_pump(&deferred));
        assert!(!super::take_deferred_pump(&deferred));
        pumping.set(false);
        assert!(super::enter_pump(&pumping));
    }

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
            "ZGX_CEF_CLOSE_RACE_RESULT",
            "ZGX_CEF_STRESS_RESULT",
            "ZGX_CEF_NAV_EVAL_RACE_RESULT",
            "ZGX_CEF_POPUP_RESULT",
        ] {
            assert!(mock_keychain_requested(|name| name == fixture), "{fixture}");
        }
        assert!(!mock_keychain_requested(|_| false));
    }

    #[test]
    fn mock_keychain_requires_a_fixture_binary_and_temporary_profile_data() {
        let temp_root = std::env::temp_dir();
        let profile = temp_root.join(format!(
            "zgx-cef-keychain-test-{}-{:?}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&profile).unwrap();

        assert!(mock_keychain_allowed(
            Some("cef_web_view"),
            Some(&profile),
            &temp_root,
            true,
        ));
        assert!(mock_keychain_allowed(
            Some("browser_profile_fixture"),
            Some(&profile),
            &temp_root,
            true,
        ));
        assert!(!mock_keychain_allowed(
            Some("Polyscale"),
            Some(&profile),
            &temp_root,
            true,
        ));
        assert!(!mock_keychain_allowed(
            Some("cef_web_view"),
            Some(Path::new("/")),
            &temp_root,
            true,
        ));
        assert!(!mock_keychain_allowed(
            Some("cef_web_view"),
            Some(&profile),
            &temp_root,
            false,
        ));

        fs::remove_dir_all(profile).unwrap();
    }

    #[test]
    fn close_race_diagnostics_are_limited_to_temporary_fixture_runs() {
        let temp_root = std::env::temp_dir();
        let directory = temp_root.join(format!(
            "zgx-cef-close-trace-test-{}-{:?}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let data_dir = directory.join("data");
        let output_dir = directory.join("logs");
        fs::create_dir_all(&data_dir).unwrap();
        fs::create_dir_all(&output_dir).unwrap();

        assert!(close_race_trace_allowed(
            Some("cef_web_view"),
            Some(&data_dir),
            Some(&output_dir),
            &temp_root,
            true,
        ));
        assert!(!close_race_trace_allowed(
            Some("Polyscale"),
            Some(&data_dir),
            Some(&output_dir),
            &temp_root,
            true,
        ));
        assert!(!close_race_trace_allowed(
            Some("cef_web_view"),
            Some(Path::new("/")),
            Some(&output_dir),
            &temp_root,
            true,
        ));
        assert!(!close_race_trace_allowed(
            Some("cef_web_view"),
            Some(&data_dir),
            Some(Path::new("/")),
            &temp_root,
            true,
        ));
        assert!(!close_race_trace_allowed(
            Some("cef_web_view"),
            Some(&data_dir),
            Some(&output_dir),
            &temp_root,
            false,
        ));

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn pump_scheduler_is_silent_after_cef_shutdown() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = calls.clone();
        let scheduler: super::PumpScheduler = std::sync::Arc::new(move |_| {
            observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });

        assert!(!schedule_while_running(true, Some(&scheduler), 0));
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert!(schedule_while_running(false, Some(&scheduler), 12));
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn permission_decisions_are_origin_and_profile_scoped_and_persisted() {
        let root = std::env::temp_dir().join(format!(
            "zgx-cef-permission-test-{}-{:?}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("site-permissions.json");
        let mut first = super::SitePermissionStore {
            decisions: Default::default(),
            path: Some(path.clone()),
        };
        assert_eq!(first.decision("https://meet.example", "camera"), None);
        first
            .remember("https://meet.example", "camera", true)
            .unwrap();
        first
            .remember("https://other.example", "notifications", false)
            .unwrap();
        assert_eq!(first.decision("https://meet.example", "camera"), Some(true));
        assert_eq!(first.decision("https://meet.example", "microphone"), None);
        assert_eq!(first.decision("https://other.example", "camera"), None);

        let restored: HashMap<String, bool> =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let mut restored = super::SitePermissionStore {
            decisions: restored,
            path: Some(path.clone()),
        };
        assert_eq!(
            restored.decision("https://meet.example", "camera"),
            Some(true)
        );
        restored.forget_origin("https://meet.example").unwrap();
        assert_eq!(restored.decision("https://meet.example", "camera"), None);
        assert_eq!(
            restored.decision("https://other.example", "notifications"),
            Some(false)
        );
        let other_profile = super::SitePermissionStore {
            decisions: Default::default(),
            path: None,
        };
        assert_eq!(
            other_profile.decision("https://meet.example", "camera"),
            None
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn permission_grants_do_not_cross_origin_or_capability_boundaries() {
        let mut store = super::SitePermissionStore {
            decisions: Default::default(),
            path: None,
        };
        store
            .remember("https://meet.example", "camera", true)
            .unwrap();

        assert_eq!(store.decision("https://meet.example", "camera"), Some(true));
        assert_eq!(
            store.decision("https://meet.example.evil", "camera"),
            None,
            "a lookalike host must not inherit another origin's grant"
        );
        assert_eq!(
            store.decision("http://meet.example", "camera"),
            None,
            "HTTP and HTTPS are distinct origins"
        );
        assert_eq!(
            store.decision("https://meet.example:8443", "camera"),
            None,
            "a different port is a different origin"
        );
        assert_eq!(
            store.decision("https://meet.example", "microphone"),
            None,
            "camera approval must not authorize microphone access"
        );
    }

    #[test]
    fn tracks_pending_creations_and_actual_browser_ids() {
        assert!(!has_browsers());
        let creation = browser_creation_started();
        assert!(
            has_browsers(),
            "an accepted creation must keep shutdown alive"
        );

        assert!(browser_created(creation, 9_314_159));
        assert!(has_browsers(), "the actual CEF ID must stay tracked");
        assert!(
            super::shutdown().is_err(),
            "shutdown must wait for on_before_close"
        );
        browser_closed(9_314_159);
        assert!(!has_browsers(), "only on_before_close releases the browser");

        let failed = browser_creation_started();
        assert!(browser_creation_failed(failed));
        assert!(
            !browser_creation_failed(failed),
            "failure is accounted once"
        );
        assert!(
            !has_browsers(),
            "rejected creation must release its pending slot"
        );
        let creation = browser_creation_started();
        assert!(browser_creation_cancelled(creation));
        assert!(
            has_browsers(),
            "canceled accepted creation still awaits CEF"
        );
        assert!(!browser_creation_cancelled(creation));
        assert!(
            super::shutdown().is_err(),
            "CEF cannot shut down while a canceled request can still callback"
        );

        assert!(
            !browser_created(creation, 9_314_160),
            "a late browser must be identified as belonging to a canceled creation"
        );
        assert!(
            has_browsers(),
            "even a late browser must prevent CEF shutdown until it closes"
        );
        assert!(!browser_creation_cancelled(creation));
        browser_closed(9_314_160);
        assert!(!has_browsers());
    }
}

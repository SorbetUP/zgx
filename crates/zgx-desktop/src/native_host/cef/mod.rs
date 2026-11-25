mod bridge;
mod browser;
mod client;
mod rendering;
mod runtime;

use zgx::web_view::PageViews;

pub(crate) fn page_views(
    dialogs: crate::FileDialogs,
    tasks: crate::TaskSpawner,
    windows: crate::WindowFactory,
) -> PageViews {
    PageViews::new(move |web| {
        Some(browser::page_view(
            web,
            dialogs.clone(),
            tasks.clone(),
            windows.clone(),
        ))
    })
}

pub(crate) fn pump() {
    runtime::pump();
}
pub(crate) fn shutdown() -> Result<(), String> {
    runtime::shutdown()
}
pub(crate) fn begin_shutdown() {
    runtime::begin_shutdown();
}
pub(crate) fn execute_process() -> Result<Option<i32>, String> {
    runtime::execute_process()
}
pub(crate) fn execute_helper_process() -> Result<Option<i32>, String> {
    runtime::execute_helper_process()
}
#[cfg(target_os = "windows")]
pub(crate) fn execute_process_with_windows_context(
    instance: cef::sys::HINSTANCE,
    sandbox_info: *mut u8,
) -> Result<Option<i32>, String> {
    runtime::execute_process_with_windows_context(instance, sandbox_info)
}
pub(crate) fn set_scheduler(schedule: std::sync::Arc<dyn Fn(i64)>) {
    runtime::set_scheduler(schedule);
}
pub(crate) fn schedule_work(delay_ms: i64) {
    runtime::schedule_work(delay_ms);
}
pub(crate) fn has_browsers() -> bool {
    runtime::has_browsers()
}

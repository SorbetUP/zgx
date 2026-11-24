mod browser;
mod client;
mod rendering;
mod runtime;

use zgx::web_view::PageViews;

pub(crate) fn page_views() -> PageViews {
    PageViews::new(|web| Some(browser::page_view(web)))
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
pub(crate) fn has_browsers() -> bool {
    runtime::has_browsers()
}

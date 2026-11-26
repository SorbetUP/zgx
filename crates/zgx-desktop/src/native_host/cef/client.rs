use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    io::Write,
    rc::Rc,
    sync::{Arc, Mutex},
};

use cef::{self, *};

use super::runtime::PermissionStoreHandle;
use super::{
    rendering::{SharedFrame, SharedPopup},
    runtime,
};

pub(super) fn trace_popup_fixture(message: &str) {
    if let Some(path) = std::env::var_os("ZGX_CEF_POPUP_RESULT").map(std::path::PathBuf::from) {
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path.with_extension("diagnostic"))
            .and_then(|mut file| writeln!(file, "{message}"));
    }
}

pub(super) type HtmlDocuments = Arc<Mutex<HashMap<String, Arc<Vec<u8>>>>>;
type HtmlHandlerKeepAlive = Arc<Mutex<HashMap<u64, ResourceRequestHandler>>>;

pub(super) type FindReplies = Rc<RefCell<VecDeque<FindReply>>>;
type FindReply = Box<dyn FnOnce(zgx::web_view::FindResult)>;

#[derive(Clone)]
pub(super) struct RenderState {
    pub frame: SharedFrame,
    pub popup: SharedPopup,
    pub size: Arc<Mutex<(i32, i32)>>,
    pub scale: Arc<Mutex<f32>>,
    pub invalidate: Rc<dyn Fn()>,
}

#[derive(Clone)]
pub(super) struct BrowserCallbacks {
    pub created: Rc<dyn Fn(Browser)>,
    pub before_close: Rc<dyn Fn(i32)>,
    pub find_replies: FindReplies,
    pub web: zgx::web_view::WeakWebView,
    pub new_windows: bool,
    pub navigation_started: Rc<dyn Fn()>,
}

wrap_download_handler! {
    pub(super) struct CEFDownloadHandler {
        web: zgx::web_view::WeakWebView,
        // The window that opened this one (popups): a child page with no
        // download handler of its own relays to it, keeping provenance.
        owner: Option<zgx::web_view::WeakWebView>,
        statuses: Rc<RefCell<HashMap<u32, zgx::web_view::DownloadStatusFn>>>,
        cancel_paths: Rc<RefCell<HashMap<u32, std::path::PathBuf>>>,
    }
    impl DownloadHandler {
        fn can_download(
            &self,
            _browser: Option<&mut Browser>,
            _url: Option<&CefString>,
            _request_method: Option<&CefString>,
        ) -> i32 {
            1
        }

        fn on_before_download(
            &self,
            _browser: Option<&mut Browser>,
            item: Option<&mut DownloadItem>,
            suggested_name: Option<&CefString>,
            callback: Option<&mut BeforeDownloadCallback>,
        ) -> i32 {
            let (Some(item), Some(callback)) = (item, callback) else {
                return 0;
            };
            let id = item.id();
            let url = CefString::from(&item.url()).to_string();
            let mime = CefString::from(&item.mime_type()).to_string();
            let name = suggested_name
                .map(ToString::to_string)
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| CefString::from(&item.suggested_file_name()).to_string());
            let web = self.web.upgrade().filter(|web| web.has_download_handler());
            // A popup child has no handler of its own (nothing mounts it);
            // relay the download to the window that opened it so the person
            // still decides where the file goes.
            let web = web.or_else(|| {
                self.owner
                    .as_ref()
                    .and_then(|owner| owner.upgrade())
                    .filter(|owner| owner.has_download_handler())
            });
            let Some(web) = web else {
                // CEF 156's BeforeDownloadCallback has no cancel method and an
                // empty path means its default temp directory. Give the
                // download a private, disposable path and cancel it from the
                // first DownloadItemCallback, the supported cancellation API.
                continue_to_cancelled_path(id, &callback, &self.cancel_paths);
                return 1;
            };

            let callback = callback.clone();
            let statuses = self.statuses.clone();
            let cancel_paths = self.cancel_paths.clone();
            web.download(
                zgx::web_view::DownloadRequest::new(url, name, mime),
                zgx::web_view::DownloadResponder::new(move |selection| match selection {
                    Some((path, status)) => {
                        statuses.borrow_mut().insert(id, status);
                        let path = path.to_string_lossy().into_owned();
                        let path: CefString = path.as_str().into();
                        callback.cont(Some(&path), 0);
                    }
                    // BeforeDownloadCallback::Continue cannot cancel in CEF
                    // 156. Do not pass an empty path (which silently writes
                    // to the temp directory); use a private target and cancel
                    // via DownloadItemCallback as soon as CEF exposes it.
                    None => continue_to_cancelled_path(id, &callback, &cancel_paths),
                }),
            );
            1
        }

        fn on_download_updated(
            &self,
            _browser: Option<&mut Browser>,
            item: Option<&mut DownloadItem>,
            callback: Option<&mut DownloadItemCallback>,
        ) {
            let Some(item) = item else { return };
            let id = item.id();
            if let Some(path) = self.cancel_paths.borrow().get(&id).cloned() {
                if let Some(callback) = callback {
                    callback.cancel();
                }
                if item.is_canceled() != 0 || item.is_interrupted() != 0 || item.is_complete() != 0 {
                    self.cancel_paths.borrow_mut().remove(&id);
                    remove_cancelled_download_files(&path);
                }
                return;
            }
            if item.is_complete() != 0 {
                update_download_status(&mut self.statuses.borrow_mut(), id, DownloadUpdate::Finished);
            } else if item.is_canceled() != 0 {
                update_download_status(
                    &mut self.statuses.borrow_mut(),
                    id,
                    DownloadUpdate::Failed("Download cancelled".into()),
                );
            } else if item.is_interrupted() != 0 {
                let reason = format!("{:?}", item.interrupt_reason());
                update_download_status(
                    &mut self.statuses.borrow_mut(),
                    id,
                    DownloadUpdate::Failed(reason),
                );
            } else {
                let total = item.total_bytes();
                update_download_status(
                    &mut self.statuses.borrow_mut(),
                    id,
                    DownloadUpdate::Progress {
                        received: item.received_bytes().max(0) as u64,
                        total: (total >= 0).then_some(total as u64),
                    },
                );
            }
        }
    }
}

impl Drop for CEFDownloadHandler {
    fn drop(&mut self) {
        for path in self.cancel_paths.borrow().values() {
            remove_cancelled_download_files(path);
        }
    }
}

fn continue_to_cancelled_path(
    id: u32,
    callback: &BeforeDownloadCallback,
    cancel_paths: &Rc<RefCell<HashMap<u32, std::path::PathBuf>>>,
) {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let path = std::env::temp_dir().join(format!(
        "polyscale-cancelled-{}-{id}-{nonce}.part",
        std::process::id()
    ));
    cancel_paths.borrow_mut().insert(id, path.clone());
    let path_string: CefString = path.to_string_lossy().as_ref().into();
    callback.cont(Some(&path_string), 0);
}

fn remove_cancelled_download_files(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let mut partial = path.as_os_str().to_os_string();
    partial.push(".crdownload");
    let _ = std::fs::remove_file(std::path::PathBuf::from(partial));
}

wrap_audio_handler! {
    pub(super) struct CEFAudioHandler {
        web: zgx::web_view::WeakWebView,
    }
    impl AudioHandler {
        fn on_audio_stream_started(
            &self,
            _browser: Option<&mut Browser>,
            _params: Option<&AudioParameters>,
            _channels: i32,
        ) {
            emit_native_audio_activity(&self.web, true);
        }

        fn on_audio_stream_stopped(&self, _browser: Option<&mut Browser>) {
            emit_native_audio_activity(&self.web, false);
        }

        fn on_audio_stream_error(&self, _browser: Option<&mut Browser>, _message: Option<&CefString>) {
            emit_native_audio_activity(&self.web, false);
        }
    }
}

fn emit_native_audio_activity(web: &zgx::web_view::WeakWebView, active: bool) {
    emit_native_activity(web, "native-audio".into(), active);
}

pub(super) fn emit_native_activity(web: &zgx::web_view::WeakWebView, reason: String, active: bool) {
    let web = web.clone();
    zgx::native_view::post(move || {
        if let Some(web) = web.upgrade() {
            // Typed native-only event instead of a fake-origin Message: this
            // never comes from the page message channel, so the page cannot
            // forge it. zgx-script maps it onto the Luau `on_message` callback
            // with the reserved "native://cef" origin for compatibility.
            web.emit(zgx::web_view::WebViewEvent::ActivityChanged { active, reason });
        }
    });
}

enum DownloadUpdate {
    Progress { received: u64, total: Option<u64> },
    Finished,
    Failed(String),
}

fn update_download_status(
    statuses: &mut HashMap<u32, zgx::web_view::DownloadStatusFn>,
    id: u32,
    update: DownloadUpdate,
) {
    use zgx::web_view::DownloadStatus;
    match update {
        DownloadUpdate::Progress { received, total } => {
            if let Some(status) = statuses.get_mut(&id) {
                status(DownloadStatus::Progress { received, total });
            }
        }
        DownloadUpdate::Finished => {
            if let Some(mut status) = statuses.remove(&id) {
                status(DownloadStatus::Finished);
            }
        }
        DownloadUpdate::Failed(reason) => {
            if let Some(mut status) = statuses.remove(&id) {
                status(DownloadStatus::Failed(reason));
            }
        }
    }
}

wrap_life_span_handler! {
    pub(super) struct CEFLifeSpanHandler {
        callbacks: BrowserCallbacks,
        profile: String,
        store: PermissionStoreHandle,
        dialogs: crate::FileDialogs,
        tasks: crate::TaskSpawner,
        windows: crate::WindowFactory,
        pending_popups: Rc<RefCell<HashMap<i32, PopupPending>>>,
    }
    impl LifeSpanHandler {
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            popup_id: i32,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: i32,
            _popup_features: Option<&PopupFeatures>,
            window_info: Option<&mut WindowInfo>,
            client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            extra_info: Option<&mut Option<DictionaryValue>>,
            no_javascript_access: Option<&mut i32>,
        ) -> i32 {
            let url = target_url.map(ToString::to_string).unwrap_or_default();
            let no_javascript_access = no_javascript_access.as_deref().copied();
            trace_popup_fixture(&format!(
                "CEF_BEFORE_POPUP:{popup_id}:{url}:no_javascript_access={no_javascript_access:?}"
            ));
            if !self.callbacks.new_windows {
                return 1;
            }
            let event_url = url.clone();
            let web = self.callbacks.web.clone();
            zgx::native_view::post(move || {
                if let Some(web) = web.upgrade() {
                    web.emit(zgx::web_view::WebViewEvent::NewWindowRequested { url: event_url });
                }
            });
            let (Some(window_info), Some(client)) = (window_info, client) else { return 1 };
            // A popup is a distinct CEF browser process context. Propagate the
            // owning WebView's renderer configuration so messages and user
            // scripts work in the child just as they do in the parent.
            if let (Some(extra_info), Some(web)) = (extra_info, self.callbacks.web.upgrade()) {
                *extra_info = super::bridge::extra_info(web.options());
            }
            let creation_id = runtime::browser_creation_started();
            let Some(prepared) = super::browser::prepare_popup(
                self.callbacks.web.clone(),
                self.profile.clone(),
                self.store.clone(),
                self.dialogs.clone(),
                self.tasks.clone(),
                self.windows.clone(),
                popup_id,
                creation_id,
                url.clone(),
                self.pending_popups.clone(),
            ) else {
                runtime::browser_creation_failed(creation_id);
                return 1;
            };
            self.pending_popups.borrow_mut().insert(
                popup_id,
                PopupPending { creation_id, window: prepared.window },
            );
            *window_info = prepared.window_info;
            *client = Some(prepared.client);
            0
        }

        fn on_before_popup_aborted(&self, _browser: Option<&mut Browser>, popup_id: i32) {
            if let Some(pending) = self.pending_popups.borrow_mut().remove(&popup_id) {
                runtime::browser_creation_failed(pending.creation_id);
                runtime::release_child_web(pending.creation_id);
                pending.window.close();
            }
        }

        fn on_after_created(&self, browser: Option<&mut Browser>) {
            if let Some(browser) = browser {
                super::cef_trace!(&format!(
                    "CEF_ON_AFTER_CREATED browser={}",
                    browser.identifier()
                ));
                (self.callbacks.created)(browser.clone());
            }
        }
        fn on_before_close(&self, browser: Option<&mut Browser>) {
            if let Some(browser) = browser {
                super::cef_trace!(&format!(
                    "CEF_ON_BEFORE_CLOSE browser={}",
                    browser.identifier()
                ));
                (self.callbacks.before_close)(browser.identifier());
            }
        }
    }
}

wrap_render_handler! {
    pub(super) struct CEFRenderHandler {
        state: RenderState,
        drag_start: Rc<dyn Fn(&DragData, i32, i32) -> bool>,
    }
    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            if let Some(rect) = rect {
                let size = *self.state.size.lock().unwrap_or_else(|poison| poison.into_inner());
                rect.x = 0;
                rect.y = 0;
                rect.width = size.0.max(1);
                rect.height = size.1.max(1);
            }
        }

        fn screen_info(&self, _browser: Option<&mut Browser>, info: Option<&mut ScreenInfo>) -> i32 {
            if let Some(info) = info {
                let (width, height) = *self.state.size.lock().unwrap_or_else(|poison| poison.into_inner());
                info.device_scale_factor = *self.state.scale.lock().unwrap_or_else(|poison| poison.into_inner());
                info.depth = 32;
                info.depth_per_component = 8;
                info.is_monochrome = 0;
                info.rect = Rect { x: 0, y: 0, width: width.max(1), height: height.max(1) };
                info.available_rect = info.rect.clone();
                return 1;
            }
            0
        }

        fn on_popup_show(&self, _browser: Option<&mut Browser>, show: i32) {
            let mut popup = self.state.popup.lock().unwrap_or_else(|poison| poison.into_inner());
            popup.visible = show != 0;
            if !popup.visible {
                popup.frame.width = 0;
                popup.frame.height = 0;
                popup.frame.pixels.clear();
            }
            drop(popup);
            (self.state.invalidate)();
        }

        fn on_popup_size(&self, _browser: Option<&mut Browser>, rect: Option<&Rect>) {
            let Some(rect) = rect else { return };
            let mut popup = self.state.popup.lock().unwrap_or_else(|poison| poison.into_inner());
            popup.rect = (rect.x, rect.y, rect.width.max(0), rect.height.max(0));
            drop(popup);
            (self.state.invalidate)();
        }

        fn on_paint(
            &self,
            _browser: Option<&mut Browser>,
            kind: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: i32,
            height: i32,
        ) {
            if buffer.is_null() || width <= 0 || height <= 0 {
                return;
            }
            let (width, height) = (width as u32, height as u32);
            let Some(length) = (width as usize).checked_mul(height as usize).and_then(|pixels| pixels.checked_mul(4)) else {
                return;
            };
            // CEF owns `buffer`; copy it before returning from this callback.
            let pixels = unsafe { std::slice::from_raw_parts(buffer, length) };
            if kind == PaintElementType::POPUP {
                let mut popup = self.state.popup.lock().unwrap_or_else(|poison| poison.into_inner());
                popup.frame.width = width;
                popup.frame.height = height;
                popup.frame.pixels.clear();
                popup.frame.pixels.extend_from_slice(pixels);
                popup.frame.revision = popup.frame.revision.wrapping_add(1);
                drop(popup);
                (self.state.invalidate)();
                return;
            }
            if kind != PaintElementType::VIEW {
                return;
            }
            let mut frame = self.state.frame.lock().unwrap_or_else(|poison| poison.into_inner());
            frame.width = width;
            frame.height = height;
            frame.pixels.clear();
            frame.pixels.extend_from_slice(pixels);
            frame.revision = frame.revision.wrapping_add(1);
            drop(frame);
            (self.state.invalidate)();
        }

        fn start_dragging(
            &self,
            _browser: Option<&mut Browser>,
            drag_data: Option<&mut DragData>,
            _allowed_ops: DragOperationsMask,
            x: i32,
            y: i32,
        ) -> i32 {
            let Some(drag_data) = drag_data else { return 0 };
            (self.drag_start)(drag_data, x, y) as i32
        }
    }
}

wrap_load_handler! {
    pub(super) struct CEFLoadHandler { web: zgx::web_view::WeakWebView, navigation_started: Rc<dyn Fn()>, profile: String }
    impl LoadHandler {
        fn on_load_start(&self, _browser: Option<&mut Browser>, frame: Option<&mut cef::Frame>, _transition: TransitionType) {
            let Some(frame) = frame.filter(|frame| frame.is_main() != 0) else { return };
            (self.navigation_started)();
            let url = CefString::from(&frame.url()).to_string();
            let web = self.web.clone();
            zgx::native_view::post(move || {
                if let Some(web) = web.upgrade() {
                    web.update_state(|state| {
                        state.url = url.clone();
                        state.loading = true;
                        state.progress = 0.05;
                        state.error = None;
                    });
                    web.emit(zgx::web_view::WebViewEvent::NavigationStarted { url: url.clone() });
                    web.emit(zgx::web_view::WebViewEvent::NavigationCommitted { url });
                }
            });
        }
        fn on_load_end(&self, browser: Option<&mut Browser>, frame: Option<&mut cef::Frame>, _status: i32) {
            let Some(frame) = frame.filter(|frame| frame.is_main() != 0) else { return };
            let url = CefString::from(&frame.url()).to_string();
            // Remember this profile's visited origin so site-data removal can
            // cover every origin of the site, not only the one asked for.
            if let Some(origin) = origin_from_url(&url)
                && (origin.starts_with("http://") || origin.starts_with("https://"))
            {
                super::runtime::record_visited_origin(&self.profile, origin);
            }
            let (can_go_back, can_go_forward) = browser
                .map(|browser| (browser.can_go_back() != 0, browser.can_go_forward() != 0))
                .unwrap_or_default();
            let web = self.web.clone();
            zgx::native_view::post(move || {
                if let Some(web) = web.upgrade() {
                    web.update_state(|state| {
                        state.url = url.clone();
                        state.loading = false;
                        state.progress = 1.0;
                        state.can_go_back = can_go_back;
                        state.can_go_forward = can_go_forward;
                        state.error = None;
                    });
                    web.emit(zgx::web_view::WebViewEvent::NavigationFinished { url });
                }
            });
        }
        fn on_load_error(&self, _browser: Option<&mut Browser>, frame: Option<&mut cef::Frame>, code: Errorcode, text: Option<&CefString>, url: Option<&CefString>) {
            let Some(frame) = frame.filter(|frame| frame.is_main() != 0) else { return };
            let url = url.map(ToString::to_string).unwrap_or_else(|| CefString::from(&frame.url()).to_string());
            if is_user_cancelled_load(code) {
                let web = self.web.clone();
                zgx::native_view::post(move || {
                    if let Some(web) = web.upgrade() {
                        web.update_state(|state| {
                            state.loading = false;
                            state.progress = 0.0;
                            state.error = None;
                        });
                    }
                });
                return;
            }
            let error = format!("{code:?}: {}", text.map(ToString::to_string).unwrap_or_default());
            let web = self.web.clone();
            zgx::native_view::post(move || {
                if let Some(web) = web.upgrade() {
                    web.update_state(|state| {
                        state.url = url.clone();
                        state.loading = false;
                        state.error = Some(error.clone());
                    });
                    web.emit(zgx::web_view::WebViewEvent::NavigationFailed { url, error });
                }
            });
        }
    }
}

fn is_user_cancelled_load(code: Errorcode) -> bool {
    code == Errorcode::ABORTED
}

wrap_display_handler! {
    pub(super) struct CEFDisplayHandler { web: zgx::web_view::WeakWebView }
    impl DisplayHandler {
        fn on_loading_progress_change(&self, _browser: Option<&mut Browser>, progress: f64) {
            let web = self.web.clone();
            let progress = progress.clamp(0.0, 1.0);
            zgx::native_view::post(move || {
                if let Some(web) = web.upgrade() {
                    web.update_state(|state| {
                        if state.loading {
                            state.progress = progress;
                        }
                    });
                }
            });
        }

        fn on_address_change(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            url: Option<&CefString>,
        ) {
            let Some(frame) = frame.filter(|frame| frame.is_main() != 0) else { return };
            let url = url.map(ToString::to_string).unwrap_or_else(|| CefString::from(&frame.url()).to_string());
            let web = self.web.clone();
            zgx::native_view::post(move || {
                if let Some(web) = web.upgrade() {
                    web.update_state(|state| state.url = url.clone());
                    web.emit(zgx::web_view::WebViewEvent::UrlChanged { url });
                }
            });
        }

        fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
            let title = title.map(ToString::to_string).unwrap_or_default();
            let web = self.web.clone();
            zgx::native_view::post(move || {
                if let Some(web) = web.upgrade() {
                    web.update_state(|state| state.title = title.clone());
                    web.emit(zgx::web_view::WebViewEvent::TitleChanged { title });
                }
            });
        }
    }
}

wrap_find_handler! {
    pub(super) struct CEFFindHandler { replies: FindReplies }
    impl FindHandler {
        fn on_find_result(
            &self,
            _browser: Option<&mut Browser>,
            _identifier: i32,
            count: i32,
            _selection_rect: Option<&Rect>,
            _active_match_ordinal: i32,
            final_update: i32,
        ) {
            if final_update == 0 {
                return;
            }
            if let Some(reply) = self.replies.borrow_mut().pop_front() {
                let mut result = zgx::web_view::FindResult::default();
                result.found = count > 0;
                result.matches = (!cfg!(target_os = "macos")).then_some(count.max(0) as u32);
                zgx::native_view::post(move || reply(result));
            }
        }
    }
}

/// A permission callback waiting for its one-shot decision. Kept in a
/// registry so a dismissal (or a closed browser) can settle it exactly once.
pub(super) enum PendingPermission {
    Prompt(PermissionPromptCallback),
    Media(MediaAccessCallback),
}

pub(super) type PendingPermissions = Rc<RefCell<HashMap<u64, PendingPermission>>>;

wrap_permission_handler! {
    pub(super) struct CEFPermissionHandler {
        profile: String,
        store: PermissionStoreHandle,
        dialogs: crate::FileDialogs,
        tasks: crate::TaskSpawner,
        pending: PendingPermissions,
        next_media_token: std::cell::Cell<u64>,
    }
    impl PermissionHandler {
        fn on_request_media_access_permission(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            requesting_origin: Option<&CefString>,
            requested_permissions: u32,
            callback: Option<&mut MediaAccessCallback>,
        ) -> i32 {
            let Some(callback) = callback else { return 1 };
            let callback = callback.clone();
            let Some(origin) = requesting_origin
                .map(ToString::to_string)
                .and_then(|origin| permission_origin(&origin))
            else {
                callback.cancel();
                return 1;
            };
            let audio = MediaAccessPermissionTypes::DEVICE_AUDIO_CAPTURE.get_raw();
            let video = MediaAccessPermissionTypes::DEVICE_VIDEO_CAPTURE.get_raw();
            let supported = audio | video;
            if requested_permissions == 0 || requested_permissions & !supported != 0 {
                callback.cancel();
                return 1;
            }
            let mut requests = Vec::new();
            if requested_permissions & audio != 0 {
                requests.push((audio, "microphone", "microphone"));
            }
            if requested_permissions & video != 0 {
                requests.push((video, "camera", "camera"));
            }
            let browser_id = _browser.as_ref().map(|browser| browser.identifier());
            let token = self.next_media_token.get().wrapping_add(1).max(1);
            self.next_media_token.set(token);
            self.pending
                .borrow_mut()
                .insert(token, PendingPermission::Media(callback.clone()));
            let pending = self.pending.clone();
            let profile = self.profile.clone();
            let store = self.store.clone();
            let dialogs = self.dialogs.clone();
            self.tasks.spawn(async move {
                // One-shot: take the callback out of the registry; if it is
                // gone (dismissed) or its browser closed, do not call into CEF.
                let Some(PendingPermission::Media(callback)) = pending.borrow_mut().remove(&token)
                else {
                    return;
                };
                if browser_id.is_some_and(|id| !super::runtime::browser_is_open(id)) {
                    callback.cancel();
                    return;
                }
                let mut granted = 0;
                for (flag, permission, label) in requests {
                    if request_permission(&profile, &store, &dialogs, &origin, permission, label).await {
                        granted |= flag;
                    } else {
                        callback.cancel();
                        return;
                    }
                }
                callback.cont(granted);
            });
            1
        }

        fn on_show_permission_prompt(
            &self,
            _browser: Option<&mut Browser>,
            prompt_id: u64,
            requesting_origin: Option<&CefString>,
            requested_permissions: u32,
            callback: Option<&mut PermissionPromptCallback>,
        ) -> i32 {
            let Some(callback) = callback else { return 1 };
            let callback = callback.clone();
            let Some(origin) = requesting_origin
                .map(ToString::to_string)
                .and_then(|origin| permission_origin(&origin))
            else {
                callback.cont(PermissionRequestResult::DENY);
                return 1;
            };
            // Register the callback so a dismissal settles it exactly once.
            self.pending
                .borrow_mut()
                .insert(prompt_id, PendingPermission::Prompt(callback.clone()));
            let kinds = [
                (
                    PermissionRequestTypes::CAMERA_PAN_TILT_ZOOM.get_raw(),
                    "camera",
                    "camera",
                ),
                (PermissionRequestTypes::CAMERA_STREAM.get_raw(), "camera", "camera"),
                (PermissionRequestTypes::MIC_STREAM.get_raw(), "microphone", "microphone"),
                (PermissionRequestTypes::NOTIFICATIONS.get_raw(), "notifications", "notifications"),
                (PermissionRequestTypes::GEOLOCATION.get_raw(), "location", "location"),
            ];
            let supported = kinds.iter().fold(0, |mask, (flag, _, _)| mask | flag);
            if requested_permissions == 0 || requested_permissions & !supported != 0 {
                callback.cont(PermissionRequestResult::DENY);
                return 1;
            }
            let requests: Vec<_> = kinds
                .into_iter()
                .filter(|(flag, _, _)| requested_permissions & flag != 0)
                .collect();
            let pending = self.pending.clone();
            let profile = self.profile.clone();
            let store = self.store.clone();
            let dialogs = self.dialogs.clone();
            self.tasks.spawn(async move {
                let mut accepted = true;
                for (_, permission, label) in requests {
                    if !request_permission(&profile, &store, &dialogs, &origin, permission, label).await {
                        accepted = false;
                        break;
                    }
                }
                // One-shot: only settle the prompt if it is still pending.
                if let Some(PendingPermission::Prompt(callback)) =
                    pending.borrow_mut().remove(&prompt_id)
                {
                    callback.cont(if accepted {
                        PermissionRequestResult::ACCEPT
                    } else {
                        PermissionRequestResult::DENY
                    });
                }
            });
            1
        }

        fn on_dismiss_permission_prompt(
            &self,
            _browser: Option<&mut Browser>,
            prompt_id: u64,
            result: PermissionRequestResult,
        ) {
            // The page went away before deciding: settle the callback once
            // with CEF's dismissal result so it cannot fire later.
            if let Some(PendingPermission::Prompt(callback)) =
                self.pending.borrow_mut().remove(&prompt_id)
            {
                callback.cont(result);
            }
        }
    }
}

async fn request_permission(
    profile: &str,
    store: &PermissionStoreHandle,
    dialogs: &crate::FileDialogs,
    origin: &str,
    permission: &str,
    label: &str,
) -> bool {
    if let Some(decision) = store.borrow().decision(origin, permission) {
        return decision;
    }
    let prompt = crate::PromptOptions::new(
        format!("Allow {label}?"),
        format!("{origin} is requesting access to your {label} ({profile} profile)."),
    )
    .buttons(crate::PromptButtons::YesNo);
    let result = dialogs.prompt(prompt).await;
    match result {
        Ok(crate::PromptResponse::Yes) => {
            if let Err(error) = store.borrow_mut().remember(origin, permission, true) {
                eprintln!("cannot save CEF site permission: {error}");
            }
            true
        }
        Ok(crate::PromptResponse::No) => {
            if let Err(error) = store.borrow_mut().remember(origin, permission, false) {
                eprintln!("cannot save CEF site permission: {error}");
            }
            false
        }
        _ => false,
    }
}

fn permission_origin(origin: &str) -> Option<String> {
    let origin = origin_from_url(origin)?;
    (origin.starts_with("https://") || origin.starts_with("http://")).then_some(origin)
}

wrap_dialog_handler! {
    pub(super) struct CEFDialogHandler { web: zgx::web_view::WeakWebView }
    impl DialogHandler {
        fn on_file_dialog(
            &self,
            _browser: Option<&mut Browser>,
            mode: FileDialogMode,
            _title: Option<&CefString>,
            _default_file_path: Option<&CefString>,
            _accept_filters: Option<&mut CefStringList>,
            _accept_extensions: Option<&mut CefStringList>,
            _accept_descriptions: Option<&mut CefStringList>,
            callback: Option<&mut FileDialogCallback>,
        ) -> i32 {
            let Some(callback) = callback else { return 1 };
            let callback = callback.clone();
            let Some((multiple, folder)) = file_dialog_selection(mode) else {
                callback.cancel();
                return 1;
            };
            let Some(web) = self.web.upgrade() else {
                callback.cancel();
                return 1;
            };
            web.choose_files(multiple, folder, move |selection| match selection {
                Some(files) if !files.is_empty() => {
                    let mut paths = CefStringList::new();
                    for file in files {
                        if !paths.append(&file.to_string_lossy()) {
                            callback.cancel();
                            return;
                        }
                    }
                    callback.cont(Some(&mut paths));
                }
                _ => callback.cancel(),
            });
            1
        }
    }
}

fn file_dialog_selection(mode: FileDialogMode) -> Option<(bool, bool)> {
    if mode == FileDialogMode::OPEN {
        Some((false, false))
    } else if mode == FileDialogMode::OPEN_MULTIPLE {
        Some((true, false))
    } else if mode == FileDialogMode::OPEN_FOLDER {
        Some((false, true))
    } else {
        None
    }
}

#[derive(Clone)]
struct CEFPolicyHandlers {
    find: FindHandler,
    permission: PermissionHandler,
    dialog: DialogHandler,
}

pub(super) fn normalize_document_url(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty()
        || !scheme
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
    {
        return None;
    }
    let scheme = scheme.to_ascii_lowercase();
    let without_fragment = rest.split('#').next()?;
    if scheme == "file" {
        return Some(format!("file://{without_fragment}"));
    }
    let path_start = without_fragment.find(['/', '?']);
    let (authority, path) = match path_start {
        Some(index) => (
            &without_fragment[..index],
            if without_fragment.as_bytes()[index] == b'?' {
                format!("/{}", &without_fragment[index..])
            } else {
                without_fragment[index..].to_owned()
            },
        ),
        None => (without_fragment, "/".to_owned()),
    };
    if authority.is_empty() || authority.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return None;
    }
    let (userinfo, host_port) = authority
        .rsplit_once('@')
        .map(|(userinfo, host)| (Some(userinfo), host))
        .unwrap_or((None, authority));
    let host_port = host_port.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "http" => Some(":80"),
        "https" => Some(":443"),
        _ => None,
    };
    let host_port = default_port
        .filter(|port| host_port.ends_with(port))
        .map(|port| host_port[..host_port.len() - port.len()].to_owned())
        .unwrap_or(host_port);
    let authority = userinfo
        .map(|userinfo| format!("{userinfo}@{host_port}"))
        .unwrap_or(host_port);
    Some(format!("{scheme}://{authority}{path}"))
}

fn next_document_chunk<'a>(body: &'a [u8], offset: &mut usize, limit: usize) -> &'a [u8] {
    let start = (*offset).min(body.len());
    let end = start.saturating_add(limit).min(body.len());
    *offset = end;
    &body[start..end]
}

/// Maps CEF's transition type to ZGX's navigation kind.
fn navigation_kind(transition: TransitionType) -> zgx::web_view::NavigationKind {
    navigation_kind_raw(cef::sys::cef_transition_type_t::from(transition) as u32)
}

/// Forward/back is a qualifier flag combined with the base type, so test it
/// on the raw value.
fn navigation_kind_raw(raw: u32) -> zgx::web_view::NavigationKind {
    use zgx::web_view::NavigationKind;
    let forward_back =
        cef::sys::cef_transition_type_t::from(TransitionType::FORWARD_BACK_FLAG) as u32;
    if raw & forward_back != 0 {
        return NavigationKind::BackForward;
    }
    let source_mask = cef::sys::cef_transition_type_t::from(TransitionType::SOURCE_MASK) as u32;
    let base = raw & source_mask;
    if base == cef::sys::cef_transition_type_t::from(TransitionType::LINK) as u32 {
        NavigationKind::Link
    } else if base == cef::sys::cef_transition_type_t::from(TransitionType::FORM_SUBMIT) as u32 {
        NavigationKind::FormSubmission
    } else if base == cef::sys::cef_transition_type_t::from(TransitionType::RELOAD) as u32 {
        NavigationKind::Reload
    } else {
        NavigationKind::Other
    }
}

wrap_request_handler! {
    struct CEFRequestHandler { html_documents: HtmlDocuments, handlers: HtmlHandlerKeepAlive, web: zgx::web_view::WeakWebView }
    impl RequestHandler {
        fn on_before_browse(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _user_gesture: i32,
            _is_redirect: i32,
        ) -> i32 {
            // Answer synchronously on the UI thread, as the policy requires:
            // cancel (1) when the controller's navigation policy refuses.
            let (Some(frame), Some(request)) = (frame, request) else {
                return 1;
            };
            let Some(web) = self.web.upgrade() else {
                return 1;
            };
            let destination = CefString::from(&request.url()).to_string();
            let navigation = zgx::web_view::NavigationRequest {
                url: destination,
                main_frame: frame.is_main() != 0,
                kind: navigation_kind(request.transition_type()),
                source_url: CefString::from(&frame.url()).to_string(),
            };
            i32::from(!web.allows(&navigation))
        }

        fn resource_request_handler(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _is_navigation: i32,
            _is_download: i32,
            _request_initiator: Option<&CefString>,
            disable_default_handling: Option<&mut i32>,
        ) -> Option<ResourceRequestHandler> {
            let request = request?;
            let request_id = request.identifier();
            let url = CefString::from(&request.url()).to_string();
            let url = normalize_document_url(&url)?;
            let contains = self
                .html_documents
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains_key(&url);
            if !contains {
                return None;
            }
            if let Some(disable_default_handling) = disable_default_handling {
                *disable_default_handling = 1;
            }
            let handler = CEFHTMLResourceRequestHandler::new(
                self.html_documents.clone(),
                request_id,
                Arc::downgrade(&self.handlers),
            );
            self.handlers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(request_id, handler.clone());
            Some(handler)
        }
    }
}

wrap_resource_request_handler! {
    struct CEFHTMLResourceRequestHandler {
        html_documents: HtmlDocuments,
        request_id: u64,
        handlers: std::sync::Weak<Mutex<HashMap<u64, ResourceRequestHandler>>>,
    }
    impl ResourceRequestHandler {
        fn on_before_resource_load(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            _callback: Option<&mut Callback>,
        ) -> ReturnValue {
            // cef-rs 156 defaults this callback to RV_CANCEL. A custom
            // resource handler must explicitly allow the request to reach
            // `resource_handler`.
            ReturnValue::CONTINUE
        }

        fn resource_handler(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
        ) -> Option<ResourceHandler> {
            let Some(request) = request else { return None };
            let raw_url = CefString::from(&request.url()).to_string();
            self.release_keepalive();
            let Some(url) = normalize_document_url(&raw_url) else {
                return None;
            };
            let body = self
                .html_documents
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&url)
                .cloned();
            let Some(body) = body else { return None };
            Some(CEFHTMLDocumentHandler::new(body, Arc::new(Mutex::new(0))))
        }

        fn on_resource_load_complete(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _response: Option<&mut Response>,
            _status: UrlrequestStatus,
            _received_content_length: i64,
        ) {
            if request.is_some_and(|request| request.identifier() == self.request_id) {
                self.release_keepalive();
            }
        }
    }
}

impl CEFHTMLResourceRequestHandler {
    fn release_keepalive(&self) {
        if let Some(handlers) = self.handlers.upgrade() {
            handlers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&self.request_id);
        }
    }
}

impl Drop for CEFHTMLResourceRequestHandler {
    fn drop(&mut self) {
        self.release_keepalive();
    }
}

wrap_resource_handler! {
    struct CEFHTMLDocumentHandler { body: Arc<Vec<u8>>, offset: Arc<Mutex<usize>> }
    impl ResourceHandler {
        fn open(
            &self,
            _request: Option<&mut Request>,
            handle_request: Option<&mut i32>,
            _callback: Option<&mut Callback>,
        ) -> i32 {
            if let Some(handle_request) = handle_request {
                *handle_request = 1;
            }
            1
        }

        fn response_headers(
            &self,
            response: Option<&mut Response>,
            response_length: Option<&mut i64>,
            _redirect_url: Option<&mut CefString>,
        ) {
            if let Some(response) = response {
                response.set_status(200);
                response.set_status_text(Some(&"OK".into()));
                response.set_mime_type(Some(&"text/html".into()));
                response.set_charset(Some(&"UTF-8".into()));
                response.set_header_by_name(
                    Some(&"Cache-Control".into()),
                    Some(&"no-store".into()),
                    1,
                );
            }
            if let Some(response_length) = response_length {
                *response_length = self.body.len() as i64;
            }
        }

        fn read(
            &self,
            data_out: *mut u8,
            bytes_to_read: i32,
            bytes_read: Option<&mut i32>,
            _callback: Option<&mut ResourceReadCallback>,
        ) -> i32 {
            if bytes_to_read <= 0 || data_out.is_null() {
                return 0;
            }
            let Some(bytes_read) = bytes_read else { return 0 };
            let mut offset = self.offset.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let chunk = next_document_chunk(&self.body, &mut offset, bytes_to_read as usize);
            if chunk.is_empty() {
                *bytes_read = 0;
                return 0;
            }
            // CEF provides `data_out` with capacity for `bytes_to_read` bytes.
            unsafe {
                std::ptr::copy_nonoverlapping(chunk.as_ptr(), data_out, chunk.len());
            }
            *bytes_read = chunk.len() as i32;
            1
        }
    }
}

wrap_client! {
pub(super) struct CEFClient { render: RenderHandler, life: LifeSpanHandler, load: LoadHandler, download: DownloadHandler, display: DisplayHandler, audio: AudioHandler, policy: CEFPolicyHandlers, request: RequestHandler, web: zgx::web_view::WeakWebView }
    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> { Some(self.render.clone()) }
        fn life_span_handler(&self) -> Option<LifeSpanHandler> { Some(self.life.clone()) }
        fn load_handler(&self) -> Option<LoadHandler> { Some(self.load.clone()) }
        fn download_handler(&self) -> Option<DownloadHandler> { Some(self.download.clone()) }
        fn display_handler(&self) -> Option<DisplayHandler> { Some(self.display.clone()) }
        fn audio_handler(&self) -> Option<AudioHandler> { Some(self.audio.clone()) }
        fn find_handler(&self) -> Option<FindHandler> { Some(self.policy.find.clone()) }
        fn permission_handler(&self) -> Option<PermissionHandler> { Some(self.policy.permission.clone()) }
        fn dialog_handler(&self) -> Option<DialogHandler> { Some(self.policy.dialog.clone()) }
        fn request_handler(&self) -> Option<RequestHandler> { Some(self.request.clone()) }
        fn on_process_message_received(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            source_process: ProcessId,
            message: Option<&mut ProcessMessage>,
        ) -> i32 {
            if source_process != ProcessId::RENDERER {
                return 0;
            }
            let (Some(frame), Some(message)) = (frame, message) else { return 0 };
            let message_name = CefString::from(&message.name()).to_string();
            let url_len = CefString::from(&frame.url()).to_string().len();
            super::bridge::bridge_trace(&format!(
                "browser-process-message name={message_name} main_frame={} url_len={url_len}",
                frame.is_main()
            ));
            if message_name != super::bridge::message_name() {
                return 0;
            }
            let Some(arguments) = message.argument_list() else { return 1 };
            // The renderer sends [body, security-origin]: the origin is
            // captured from the browser-computed `globalThis.origin` of the
            // sending frame's document and stored renderer-side, so the page
            // cannot forge it.
            if arguments.size() != 2 {
                super::bridge::bridge_trace("browser-process-message reject=arg-count");
                return 1;
            }
            let body = CefString::from(&arguments.string(0)).to_string();
            if body.len() > 1_048_576 || serde_json::from_str::<serde_json::Value>(&body).is_err() {
                return 1;
            }
            let origin = CefString::from(&arguments.string(1)).to_string();
            if !is_valid_security_origin(&origin) {
                super::bridge::bridge_trace("browser-process-message reject=origin");
                return 1;
            }
            // SECURITY: never derive a message's origin from frame.url().
            // about:srcdoc/about:blank inherit the parent origin, a sandboxed
            // iframe has an opaque origin even with an http(s) URL, and
            // blob: carries its creator's origin — url parsing gets all three
            // wrong and would let a sandboxed iframe pose as a trusted http
            // origin. An origin of "null" here means an opaque origin: such
            // messages must never trigger privileged native operations.
            let main_frame = frame.is_main() != 0;
            let web = self.web.clone();
            zgx::native_view::post(move || {
                if let Some(web) = web.upgrade() {
                    web.emit(zgx::web_view::WebViewEvent::Message { body, origin, main_frame });
                }
            });
            1
        }
    }
}

pub(super) fn client(
    frame: SharedFrame,
    popup: SharedPopup,
    size: Arc<Mutex<(i32, i32)>>,
    scale: Arc<Mutex<f32>>,
    invalidate: Rc<dyn Fn()>,
    callbacks: BrowserCallbacks,
    web: zgx::web_view::WeakWebView,
    download_owner: Option<zgx::web_view::WeakWebView>,
    profile: String,
    store: PermissionStoreHandle,
    dialogs: crate::FileDialogs,
    tasks: crate::TaskSpawner,
    windows: crate::WindowFactory,
    drag_start: Rc<dyn Fn(&DragData, i32, i32) -> bool>,
    html_documents: HtmlDocuments,
) -> Client {
    let state = RenderState {
        frame,
        popup,
        size,
        scale,
        invalidate,
    };
    let render = CEFRenderHandler::new(state, drag_start);
    let find_replies = callbacks.find_replies.clone();
    let navigation_started = callbacks.navigation_started.clone();
    let life = CEFLifeSpanHandler::new(
        callbacks,
        profile.clone(),
        store.clone(),
        dialogs.clone(),
        tasks.clone(),
        windows,
        Rc::new(RefCell::new(HashMap::new())),
    );
    let load = CEFLoadHandler::new(web.clone(), navigation_started, profile.clone());
    let display = CEFDisplayHandler::new(web.clone());
    let download = CEFDownloadHandler::new(
        web.clone(),
        download_owner,
        Rc::new(RefCell::new(HashMap::new())),
        Rc::new(RefCell::new(HashMap::new())),
    );
    let audio = CEFAudioHandler::new(web.clone());
    let find = CEFFindHandler::new(find_replies);
    let permission = CEFPermissionHandler::new(
        profile,
        store,
        dialogs,
        tasks,
        Rc::new(RefCell::new(HashMap::new())),
        std::cell::Cell::new(0),
    );
    let dialog = CEFDialogHandler::new(web.clone());
    CEFClient::new(
        render,
        life,
        load,
        download,
        display,
        audio,
        CEFPolicyHandlers {
            find,
            permission,
            dialog,
        },
        CEFRequestHandler::new(
            html_documents,
            Arc::new(Mutex::new(HashMap::new())),
            web.clone(),
        ),
        web,
    )
}

pub(super) struct PopupPending {
    pub(super) creation_id: u64,
    pub(super) window: crate::WindowHandle,
}

/// A security origin exactly as the browser serializes one: either the
/// opaque origin `"null"`, or `scheme://authority` (scheme starts with an
/// ASCII letter, then letters/digits/`+`/`-`/`.`; authority non-empty and
/// containing no path/query/fragment/userinfo). Used to validate the origin
/// the renderer attaches to page messages — anything else is rejected.
pub(super) fn is_valid_security_origin(origin: &str) -> bool {
    if origin == "null" {
        return true;
    }
    let Some((scheme, authority)) = origin.split_once("://") else {
        return false;
    };
    let mut bytes = scheme.bytes();
    if !bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic()) {
        return false;
    }
    if !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.')) {
        return false;
    }
    !authority.is_empty() && !authority.contains(['/', '?', '#', '@'])
}

/// Derives an origin-looking string from a URL. SECURITY: do NOT use this for
/// page-message origins — it cannot express opaque origins correctly
/// (about:srcdoc/about:blank inherit the parent, sandboxed iframes are opaque
/// despite http(s) URLs, blob: has the creator's origin). Page messages carry
/// the browser-computed origin instead (see `on_process_message_received`).
/// This remains only for CEF-supplied security origins (permission requests)
/// and visited-site bookkeeping, where CEF already hands us the real thing.
pub(super) fn origin_from_url(url: &str) -> Option<String> {
    if url.starts_with("data:") || url.starts_with("about:blank") || url.starts_with("about:srcdoc")
    {
        return Some("null".into());
    }
    let (scheme, remainder) = url.split_once("://")?;
    if scheme.is_empty()
        || !scheme
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
    {
        return None;
    }
    let authority = remainder
        .split(['/', '?', '#'])
        .next()?
        .rsplit('@')
        .next()?;
    if authority.is_empty() {
        return None;
    }
    let scheme = scheme.to_ascii_lowercase();
    let authority = authority.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "http" => Some(":80"),
        "https" => Some(":443"),
        _ => None,
    };
    let authority = default_port
        .filter(|port| authority.ends_with(port))
        .map(|port| authority[..authority.len() - port.len()].to_owned())
        .unwrap_or(authority);
    Some(format!("{scheme}://{authority}"))
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::HashMap, rc::Rc};

    use super::{
        DownloadUpdate, file_dialog_selection, is_user_cancelled_load, navigation_kind,
        navigation_kind_raw, next_document_chunk, normalize_document_url, update_download_status,
    };
    use cef::{FileDialogMode, TransitionType};
    use zgx::web_view::{DownloadStatus, NavigationKind};

    #[test]
    fn transition_type_maps_to_navigation_kind() {
        let raw = |t: TransitionType| cef::sys::cef_transition_type_t::from(t) as u32;
        let forward_back = raw(TransitionType::FORWARD_BACK_FLAG);
        assert_eq!(
            navigation_kind_raw(raw(TransitionType::LINK)),
            NavigationKind::Link
        );
        assert_eq!(
            navigation_kind_raw(raw(TransitionType::FORM_SUBMIT)),
            NavigationKind::FormSubmission
        );
        assert_eq!(
            navigation_kind_raw(raw(TransitionType::RELOAD)),
            NavigationKind::Reload
        );
        // Forward/back is a qualifier flag combined with the base type.
        assert_eq!(
            navigation_kind_raw(raw(TransitionType::LINK) | forward_back),
            NavigationKind::BackForward
        );
        assert_eq!(
            navigation_kind_raw(raw(TransitionType::AUTO_TOPLEVEL)),
            NavigationKind::Other
        );
        assert_eq!(navigation_kind(TransitionType::LINK), NavigationKind::Link);
    }

    #[test]
    fn html_document_reader_serves_bounded_chunks_and_finishes_at_eof() {
        let body = b"abcdef";
        let mut offset = 0;
        assert_eq!(next_document_chunk(body, &mut offset, 2), b"ab");
        assert_eq!(next_document_chunk(body, &mut offset, 3), b"cde");
        assert_eq!(next_document_chunk(body, &mut offset, 8), b"f");
        assert!(next_document_chunk(body, &mut offset, 8).is_empty());
        assert_eq!(offset, body.len());
    }

    #[test]
    fn html_document_url_keeps_the_base_origin_and_removes_only_fragment() {
        assert_eq!(
            normalize_document_url("HTTPS://Example.COM:443/app/page?mode=1#section"),
            Some("https://example.com/app/page?mode=1".into())
        );
        assert_eq!(
            normalize_document_url("http://127.0.0.1:43120"),
            Some("http://127.0.0.1:43120/".into())
        );
        assert_eq!(
            normalize_document_url("file:///tmp/example/page.html#section"),
            Some("file:///tmp/example/page.html".into())
        );
        assert_eq!(normalize_document_url("data:text/html,hello"), None);
        assert_eq!(normalize_document_url("https:///missing-host"), None);
    }

    #[test]
    fn download_progress_and_completion_reach_the_registered_handler_once() {
        let seen = Rc::new(RefCell::new(Vec::new()));
        let recorded = seen.clone();
        let mut statuses: HashMap<u32, zgx::web_view::DownloadStatusFn> = HashMap::new();
        statuses.insert(
            7,
            Box::new(move |status| recorded.borrow_mut().push(status)),
        );

        update_download_status(
            &mut statuses,
            7,
            DownloadUpdate::Progress {
                received: 4,
                total: Some(9),
            },
        );
        update_download_status(&mut statuses, 7, DownloadUpdate::Finished);
        update_download_status(&mut statuses, 7, DownloadUpdate::Finished);

        assert!(statuses.is_empty());
        assert_eq!(
            *seen.borrow(),
            [
                DownloadStatus::Progress {
                    received: 4,
                    total: Some(9),
                },
                DownloadStatus::Finished,
            ]
        );
    }

    #[test]
    fn cancelled_and_interrupted_downloads_finish_with_an_error() {
        let seen = Rc::new(RefCell::new(Vec::new()));
        let recorded = seen.clone();
        let mut statuses: HashMap<u32, zgx::web_view::DownloadStatusFn> = HashMap::new();
        statuses.insert(
            1,
            Box::new(move |status| recorded.borrow_mut().push(status)),
        );
        update_download_status(
            &mut statuses,
            1,
            DownloadUpdate::Failed("Download cancelled".into()),
        );
        assert_eq!(
            seen.borrow().as_slice(),
            [DownloadStatus::Failed("Download cancelled".into())]
        );

        let recorded = seen.clone();
        statuses.insert(
            2,
            Box::new(move |status| recorded.borrow_mut().push(status)),
        );
        update_download_status(
            &mut statuses,
            2,
            DownloadUpdate::Failed("NetworkFailed".into()),
        );
        assert_eq!(
            seen.borrow().as_slice(),
            [
                DownloadStatus::Failed("Download cancelled".into()),
                DownloadStatus::Failed("NetworkFailed".into()),
            ]
        );
        assert!(statuses.is_empty());
    }

    #[test]
    fn refused_download_cleanup_removes_payload_and_partial_file() {
        let root = std::env::temp_dir().join(format!(
            "zgx-cef-refused-download-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let target = root.join("refused.part");
        let partial = root.join("refused.part.crdownload");
        std::fs::write(&target, b"payload").unwrap();
        std::fs::write(&partial, b"partial").unwrap();

        super::remove_cancelled_download_files(&target);

        assert!(!target.exists());
        assert!(!partial.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn origin_from_url_is_only_for_cef_supplied_origins() {
        assert_eq!(
            super::origin_from_url("HTTPS://Example.COM:443/path?q=1"),
            Some("https://example.com".into())
        );
        assert_eq!(
            super::origin_from_url("http://127.0.0.1:4312/message"),
            Some("http://127.0.0.1:4312".into())
        );
        assert_eq!(
            super::origin_from_url("myapp://settings/profile"),
            Some("myapp://settings".into())
        );
        assert_eq!(
            super::origin_from_url("data:text/html,fixture"),
            Some("null".into())
        );
        assert_eq!(
            super::origin_from_url("data:text/plain,hello"),
            Some("null".into())
        );
        assert_eq!(super::origin_from_url("https:///missing-host"), None);
    }

    #[test]
    fn security_origins_are_null_or_scheme_authority() {
        use super::is_valid_security_origin;
        // Opaque origins (sandboxed iframes, data:/about: documents).
        assert!(is_valid_security_origin("null"));
        assert!(is_valid_security_origin("https://example.com"));
        assert!(is_valid_security_origin("http://127.0.0.1:4312"));
        assert!(is_valid_security_origin("http://[::1]:8080"));
        assert!(is_valid_security_origin("myapp://settings"));
        assert!(is_valid_security_origin("chrome-extension://abc123"));
        // Not origins: URLs with paths/userinfo, bare hosts, junk.
        assert!(!is_valid_security_origin("https://example.com/path"));
        assert!(!is_valid_security_origin("https://user@example.com"));
        assert!(!is_valid_security_origin("https://example.com?q=1"));
        assert!(!is_valid_security_origin("example.com"));
        assert!(!is_valid_security_origin("https://"));
        assert!(!is_valid_security_origin("1https://example.com"));
        assert!(!is_valid_security_origin("javascript:alert(1)"));
        assert!(!is_valid_security_origin(""));
        // Well-formed scheme://authority — the string shape alone proves
        // nothing; trust comes from the renderer-side origin map, which the
        // page cannot write to.
        assert!(is_valid_security_origin("native://cef"));
    }

    #[test]
    fn permissions_only_accept_real_http_origins() {
        assert_eq!(
            super::permission_origin("https://Meet.Example:443/room?id=1"),
            Some("https://meet.example".into())
        );
        assert_eq!(
            super::permission_origin("http://127.0.0.1:8123/"),
            Some("http://127.0.0.1:8123".into())
        );
        assert_eq!(super::permission_origin("data:text/html,hello"), None);
        assert_eq!(super::permission_origin("file:///tmp/page.html"), None);
        assert_eq!(super::permission_origin("javascript:alert(1)"), None);
    }

    #[test]
    fn file_picker_modes_are_mapped_without_implicit_save_dialogs() {
        assert_eq!(
            file_dialog_selection(FileDialogMode::OPEN),
            Some((false, false))
        );
        assert_eq!(
            file_dialog_selection(FileDialogMode::OPEN_MULTIPLE),
            Some((true, false))
        );
        assert_eq!(
            file_dialog_selection(FileDialogMode::OPEN_FOLDER),
            Some((false, true))
        );
        assert_eq!(file_dialog_selection(FileDialogMode::SAVE), None);
    }

    #[test]
    fn intentional_navigation_abort_is_not_a_page_failure() {
        assert!(is_user_cancelled_load(cef::Errorcode::ABORTED));
        assert!(!is_user_cancelled_load(cef::Errorcode::FAILED));
    }
}

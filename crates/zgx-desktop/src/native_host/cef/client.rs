use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    rc::Rc,
    sync::{Arc, Mutex},
};

use cef::{self, *};

use super::runtime::PermissionStoreHandle;
use super::{
    rendering::{SharedFrame, SharedPopup},
    runtime,
};

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
        statuses: Rc<RefCell<HashMap<u32, zgx::web_view::DownloadStatusFn>>>,
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
            let Some(web) = self.web.upgrade() else {
                callback.cont(None, 0);
                return 1;
            };

            let callback = callback.clone();
            let statuses = self.statuses.clone();
            web.download(
                zgx::web_view::DownloadRequest::new(url, name, mime),
                zgx::web_view::DownloadResponder::new(move |selection| match selection {
                    Some((path, status)) => {
                        statuses.borrow_mut().insert(id, status);
                        let path = path.to_string_lossy().into_owned();
                        let path: CefString = path.as_str().into();
                        callback.cont(Some(&path), 0);
                    }
                    None => callback.cont(None, 0),
                }),
            );
            1
        }

        fn on_download_updated(
            &self,
            _browser: Option<&mut Browser>,
            item: Option<&mut DownloadItem>,
            _callback: Option<&mut DownloadItemCallback>,
        ) {
            let Some(item) = item else { return };
            let id = item.id();
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
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut i32>,
        ) -> i32 {
            let url = target_url.map(ToString::to_string).unwrap_or_default();
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
            let settled = Rc::new(Cell::new(false));
            let Some(prepared) = super::browser::prepare_popup(
                self.callbacks.web.clone(),
                self.profile.clone(),
                self.store.clone(),
                self.dialogs.clone(),
                self.tasks.clone(),
                self.windows.clone(),
                popup_id,
                settled.clone(),
                self.pending_popups.clone(),
            ) else {
                return 1;
            };
            runtime::browser_creation_started();
            self.pending_popups.borrow_mut().insert(
                popup_id,
                PopupPending { settled, window: prepared.window },
            );
            *window_info = prepared.window_info;
            *client = Some(prepared.client);
            0
        }

        fn on_before_popup_aborted(&self, _browser: Option<&mut Browser>, popup_id: i32) {
            if let Some(pending) = self.pending_popups.borrow_mut().remove(&popup_id) {
                if !pending.settled.replace(true) {
                    runtime::browser_creation_failed();
                }
                pending.window.close();
            }
        }

        fn on_after_created(&self, browser: Option<&mut Browser>) {
            if let Some(browser) = browser {
                (self.callbacks.created)(browser.clone());
            }
        }
        fn on_before_close(&self, browser: Option<&mut Browser>) {
            if let Some(browser) = browser {
                (self.callbacks.before_close)(browser.identifier());
            }
        }
    }
}

wrap_render_handler! {
    pub(super) struct CEFRenderHandler { state: RenderState }
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
    }
}

wrap_load_handler! {
    pub(super) struct CEFLoadHandler { web: zgx::web_view::WeakWebView, navigation_started: Rc<dyn Fn()> }
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

wrap_permission_handler! {
    pub(super) struct CEFPermissionHandler {
        profile: String,
        store: PermissionStoreHandle,
        dialogs: crate::FileDialogs,
        tasks: crate::TaskSpawner,
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
            let profile = self.profile.clone();
            let store = self.store.clone();
            let dialogs = self.dialogs.clone();
            self.tasks.spawn(async move {
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
            _prompt_id: u64,
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
            let kinds = [
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
            let profile = self.profile.clone();
            let store = self.store.clone();
            let dialogs = self.dialogs.clone();
            self.tasks.spawn(async move {
                for (_, permission, label) in requests {
                    if !request_permission(&profile, &store, &dialogs, &origin, permission, label).await {
                        callback.cont(PermissionRequestResult::DENY);
                        return;
                    }
                }
                callback.cont(PermissionRequestResult::ACCEPT);
            });
            1
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

wrap_client! {
pub(super) struct CEFClient { render: RenderHandler, life: LifeSpanHandler, load: LoadHandler, download: DownloadHandler, display: DisplayHandler, policy: CEFPolicyHandlers, web: zgx::web_view::WeakWebView }
    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> { Some(self.render.clone()) }
        fn life_span_handler(&self) -> Option<LifeSpanHandler> { Some(self.life.clone()) }
        fn load_handler(&self) -> Option<LoadHandler> { Some(self.load.clone()) }
        fn download_handler(&self) -> Option<DownloadHandler> { Some(self.download.clone()) }
        fn display_handler(&self) -> Option<DisplayHandler> { Some(self.display.clone()) }
        fn find_handler(&self) -> Option<FindHandler> { Some(self.policy.find.clone()) }
        fn permission_handler(&self) -> Option<PermissionHandler> { Some(self.policy.permission.clone()) }
        fn dialog_handler(&self) -> Option<DialogHandler> { Some(self.policy.dialog.clone()) }
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
            if CefString::from(&message.name()).to_string() != super::bridge::message_name() {
                return 0;
            }
            let Some(arguments) = message.argument_list() else { return 1 };
            if arguments.size() != 1 {
                return 1;
            }
            let body = CefString::from(&arguments.string(0)).to_string();
            if body.len() > 1_048_576 || serde_json::from_str::<serde_json::Value>(&body).is_err() {
                return 1;
            }
            let url = CefString::from(&frame.url()).to_string();
            let Some(origin) = origin_from_url(&url) else { return 1 };
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
    profile: String,
    store: PermissionStoreHandle,
    dialogs: crate::FileDialogs,
    tasks: crate::TaskSpawner,
    windows: crate::WindowFactory,
) -> Client {
    let state = RenderState {
        frame,
        popup,
        size,
        scale,
        invalidate,
    };
    let render = CEFRenderHandler::new(state);
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
    let load = CEFLoadHandler::new(web.clone(), navigation_started);
    let display = CEFDisplayHandler::new(web.clone());
    let download = CEFDownloadHandler::new(web.clone(), Rc::new(RefCell::new(HashMap::new())));
    let find = CEFFindHandler::new(find_replies);
    let permission = CEFPermissionHandler::new(profile, store, dialogs, tasks);
    let dialog = CEFDialogHandler::new(web.clone());
    CEFClient::new(
        render,
        life,
        load,
        download,
        display,
        CEFPolicyHandlers {
            find,
            permission,
            dialog,
        },
        web,
    )
}

pub(super) struct PopupPending {
    pub(super) settled: Rc<Cell<bool>>,
    pub(super) window: crate::WindowHandle,
}

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
        DownloadUpdate, file_dialog_selection, is_user_cancelled_load, update_download_status,
    };
    use cef::FileDialogMode;
    use zgx::web_view::DownloadStatus;

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
    fn frame_origin_is_derived_from_native_frame_url() {
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

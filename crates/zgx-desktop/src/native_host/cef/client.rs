use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    sync::{Arc, Mutex},
};

use cef::{self, *};

use super::rendering::{SharedFrame, SharedPopup};

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
    pub(super) struct CEFLifeSpanHandler { callbacks: BrowserCallbacks }
    impl LifeSpanHandler {
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
    pub(super) struct CEFLoadHandler { web: zgx::web_view::WeakWebView }
    impl LoadHandler {
        fn on_load_start(&self, _browser: Option<&mut Browser>, frame: Option<&mut cef::Frame>, _transition: TransitionType) {
            let Some(frame) = frame.filter(|frame| frame.is_main() != 0) else { return };
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

wrap_display_handler! {
    pub(super) struct CEFDisplayHandler { web: zgx::web_view::WeakWebView }
    impl DisplayHandler {
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

wrap_client! {
pub(super) struct CEFClient { render: RenderHandler, life: LifeSpanHandler, load: LoadHandler, download: DownloadHandler, display: DisplayHandler, web: zgx::web_view::WeakWebView }
    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> { Some(self.render.clone()) }
        fn life_span_handler(&self) -> Option<LifeSpanHandler> { Some(self.life.clone()) }
        fn load_handler(&self) -> Option<LoadHandler> { Some(self.load.clone()) }
        fn download_handler(&self) -> Option<DownloadHandler> { Some(self.download.clone()) }
        fn display_handler(&self) -> Option<DisplayHandler> { Some(self.display.clone()) }
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
) -> Client {
    let state = RenderState {
        frame,
        popup,
        size,
        scale,
        invalidate,
    };
    let render = CEFRenderHandler::new(state);
    let life = CEFLifeSpanHandler::new(callbacks);
    let load = CEFLoadHandler::new(web.clone());
    let display = CEFDisplayHandler::new(web.clone());
    let download = CEFDownloadHandler::new(web.clone(), Rc::new(RefCell::new(HashMap::new())));
    CEFClient::new(render, life, load, download, display, web)
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

    use super::{DownloadUpdate, update_download_status};
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
}

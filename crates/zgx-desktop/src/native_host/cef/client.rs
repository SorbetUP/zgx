use std::sync::{Arc, Mutex};

use cef::{self, *};

use super::rendering::SharedFrame;

#[derive(Clone)]
pub(super) struct RenderState {
    pub frame: SharedFrame,
    pub size: Arc<Mutex<(i32, i32)>>,
    pub scale: Arc<Mutex<f32>>,
    pub invalidate: Arc<dyn Fn()>,
}

#[derive(Clone)]
pub(super) struct BrowserCallbacks {
    pub created: Arc<dyn Fn(Browser)>,
    pub before_close: Arc<dyn Fn()>,
}

wrap_life_span_handler! {
    pub(super) struct CEFLifeSpanHandler { callbacks: BrowserCallbacks }
    impl LifeSpanHandler {
        fn on_after_created(&self, browser: Option<&mut Browser>) {
            if let Some(browser) = browser {
                (self.callbacks.created)(browser.clone());
            }
        }
        fn on_before_close(&self, _browser: Option<&mut Browser>) { (self.callbacks.before_close)(); }
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

        fn on_popup_show(&self, _browser: Option<&mut Browser>, _show: i32) {}

        fn on_popup_size(&self, _browser: Option<&mut Browser>, _rect: Option<&Rect>) {}

        fn on_paint(
            &self,
            _browser: Option<&mut Browser>,
            kind: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: i32,
            height: i32,
        ) {
            if kind != PaintElementType::VIEW || buffer.is_null() || width <= 0 || height <= 0 {
                return;
            }
            let (width, height) = (width as u32, height as u32);
            let Some(length) = (width as usize).checked_mul(height as usize).and_then(|pixels| pixels.checked_mul(4)) else {
                return;
            };
            // CEF owns `buffer`; copy it before returning from this callback.
            let pixels = unsafe { std::slice::from_raw_parts(buffer, length) };
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
    pub(super) struct CEFLoadHandler;
    impl LoadHandler {
        fn on_load_start(&self, _browser: Option<&mut Browser>, frame: Option<&mut cef::Frame>, _transition: TransitionType) {
        }
        fn on_load_end(&self, _browser: Option<&mut Browser>, frame: Option<&mut cef::Frame>, status: i32) {
        }
        fn on_load_error(&self, _browser: Option<&mut Browser>, _frame: Option<&mut cef::Frame>, code: Errorcode, text: Option<&CefString>, url: Option<&CefString>) {
        }
    }
}

wrap_client! {
pub(super) struct CEFClient { render: RenderHandler, life: LifeSpanHandler, load: LoadHandler }
    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> { Some(self.render.clone()) }
        fn life_span_handler(&self) -> Option<LifeSpanHandler> { Some(self.life.clone()) }
        fn load_handler(&self) -> Option<LoadHandler> { Some(self.load.clone()) }
    }
}

pub(super) fn client(
    frame: SharedFrame,
    size: Arc<Mutex<(i32, i32)>>,
    scale: Arc<Mutex<f32>>,
    invalidate: Arc<dyn Fn()>,
    callbacks: BrowserCallbacks,
) -> Client {
    let state = RenderState {
        frame,
        size,
        scale,
        invalidate,
    };
    let render = CEFRenderHandler::new(state);
    let life = CEFLifeSpanHandler::new(callbacks);
    let load = CEFLoadHandler::new();
    CEFClient::new(render, life, load)
}

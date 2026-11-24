use std::sync::{Arc, Mutex};

use cef::{self, *};

use super::rendering::{Frame, SharedFrame};

#[derive(Clone)]
pub(super) struct RenderState {
    pub frame: SharedFrame,
    pub size: Arc<Mutex<(i32, i32)>>,
    pub invalidate: Arc<dyn Fn()>,
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
                info.device_scale_factor = 1.0;
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
            if kind != PaintElementType::View || buffer.is_null() || width <= 0 || height <= 0 {
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

wrap_client! {
    pub(super) struct CEFClient { render: RenderHandler }
    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> { Some(self.render.clone()) }
    }
}

pub(super) fn client(frame: SharedFrame, size: Arc<Mutex<(i32, i32)>>, invalidate: Arc<dyn Fn()>) -> Client {
    let state = RenderState { frame, size, invalidate };
    let render = CEFRenderHandler::new(state);
    CEFClient::new(render)
}

pub(super) fn empty_frame() -> Frame { Frame::default() }

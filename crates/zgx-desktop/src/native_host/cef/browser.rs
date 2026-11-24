use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{Arc, Mutex},
};

use cef::*;
use zgx::{
    compose::{View, component, div, gpu_view, text},
    input::{InputEvent, Key, Modifiers, PointerButton},
    style::Styled,
    web_view::{WebProfile, WebView, WebViewBackend, WebViewCommand},
};

use super::{
    client::{self, BrowserCallbacks},
    rendering::{Painter, SharedFrame},
    runtime,
};

pub(super) struct CefWebView(Rc<Page>);

pub(super) struct Page {
    web: zgx::web_view::WeakWebView,
    browser: Rc<RefCell<Option<Browser>>>,
    pending: RefCell<Vec<WebViewCommand>>,
    closing: Cell<bool>,
    visible: Cell<u32>,
    gpu: zgx::gpu_view::GpuView,
    frame: SharedFrame,
    size: Arc<Mutex<(i32, i32)>>,
    scale: Arc<Mutex<f32>>,
}

impl WebViewBackend for CefWebView {
    fn run(&self, command: WebViewCommand) {
        if self.0.closing.get() {
            return;
        }
        if let Some(browser) = self.0.browser.borrow().as_ref() {
            apply(browser, command);
        } else {
            self.0.pending.borrow_mut().push(command);
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl Drop for CefWebView {
    fn drop(&mut self) {
        self.0.closing.set(true);
        if let Some(browser) = self.0.browser.borrow().as_ref()
            && let Some(host) = browser.host()
        {
            host.close_browser(1);
        }
    }
}

fn apply(browser: &Browser, command: WebViewCommand) {
    match command {
        WebViewCommand::LoadUrl(url) => {
            let frame = browser.main_frame();
            if let Some(frame) = frame {
                frame.load_url(Some(&url.as_str().into()));
            }
        }
        WebViewCommand::LoadHtml { html, base_url } => {
            // A data URL keeps this first backend independent of a local HTTP
            // server; relative links use the optional base through a <base>.
            let document = if let Some(base) = base_url {
                format!("<base href=\"{}\">{}", escape_attr(&base), html)
            } else {
                html
            };
            let url = format!("data:text/html;charset=utf-8,{}", percent_encode(&document));
            let frame = browser.main_frame();
            if let Some(frame) = frame {
                frame.load_url(Some(&url.as_str().into()));
            }
        }
        WebViewCommand::Reload => browser.reload(),
        WebViewCommand::Stop => browser.stop_load(),
        WebViewCommand::GoBack => browser.go_back(),
        WebViewCommand::GoForward => browser.go_forward(),
        WebViewCommand::GoToHistory(n) => {
            if n < 0 {
                for _ in 0..n.unsigned_abs().min(100) {
                    if browser.can_go_back() == 0 {
                        break;
                    }
                    browser.go_back();
                }
            } else {
                for _ in 0..(n as u32).min(100) {
                    if browser.can_go_forward() == 0 {
                        break;
                    }
                    browser.go_forward();
                }
            }
        }
        WebViewCommand::Evaluate { reply, .. } => {
            if let Some(reply) = reply {
                reply(Err(
                    "CEF JavaScript result callbacks are not wired yet".into()
                ));
            }
        }
        WebViewCommand::Focus(focus) => {
            if let Some(host) = browser.host() {
                host.set_focus(focus as i32);
            }
        }
        WebViewCommand::OpenDevtools => {
            if let Some(host) = browser.host() {
                let info = WindowInfo {
                    windowless_rendering_enabled: 1,
                    ..Default::default()
                };
                let settings = BrowserSettings::default();
                // DevTools requires a host window in CEF OSR; report no
                // false success until the desktop overlay is implemented.
                let _ = (host, info, settings);
            }
        }
        WebViewCommand::SetZoom(zoom) => {
            if let Some(host) = browser.host() {
                host.set_zoom_level(zoom.log2());
            }
        }
    }
}

pub(super) fn backend(web: &WebView) -> Result<Rc<Page>, String> {
    if let Some(existing) = web.backend()
        && let Some(page) = existing.as_any().downcast_ref::<CefWebView>()
    {
        return Ok(page.0.clone());
    }
    runtime::initialize()?;
    let profile = match &web.options().profile {
        WebProfile::Default => "default".to_owned(),
        WebProfile::Ephemeral => format!("ephemeral-{}", web.id()),
        WebProfile::Named(id) => id.clone(),
    };
    let context = runtime::request_context(&profile)?;
    let frame = Arc::new(Mutex::default());
    let size = Arc::new(Mutex::new((1024, 768)));
    let scale = Arc::new(Mutex::new(1.));
    let browser = Rc::new(RefCell::new(None));
    let gpu = zgx_gpu::viewport::view(Painter::new(
        frame.clone(),
        size.clone(),
        scale.clone(),
        browser.clone(),
    ));
    let page = Rc::new(Page {
        web: web.downgrade(),
        browser: browser.clone(),
        pending: RefCell::new(Vec::new()),
        closing: Cell::new(false),
        visible: Cell::new(0),
        gpu,
        frame,
        size,
        scale,
    });
    let weak = Rc::downgrade(&page);
    let created_weak = weak.clone();
    let closed_weak = weak;
    let callbacks = BrowserCallbacks {
        created: Arc::new(move |browser| {
            let Some(page) = created_weak.upgrade() else {
                return;
            };
            *page.browser.borrow_mut() = Some(browser.clone());
            if let Some(host) = browser.host() {
                host.was_hidden((page.visible.get() == 0) as i32);
                host.invalidate(PaintElementType::VIEW);
            }
            let pending = std::mem::take(&mut *page.pending.borrow_mut());
            for command in pending {
                apply(&browser, command);
            }
            runtime::schedule_work(0);
            let _ = page.web.upgrade();
        }),
        before_close: Arc::new(move || {
            runtime::browser_closed();
            if let Some(page) = closed_weak.upgrade() {
                page.browser.borrow_mut().take();
            }
        }),
    };
    let invalidator = page.gpu.clone();
    let client = client::client(
        page.frame.clone(),
        page.size.clone(),
        page.scale.clone(),
        Arc::new(move || invalidator.invalidate()),
        callbacks,
        web.downgrade(),
    );
    let info = WindowInfo {
        windowless_rendering_enabled: 1,
        shared_texture_enabled: 0,
        external_begin_frame_enabled: 0,
        ..Default::default()
    };
    let settings = BrowserSettings {
        windowless_frame_rate: 30,
        ..Default::default()
    };
    let url: CefString = "about:blank".into();
    let mut context = context;
    let accepted = browser_host_create_browser(
        Some(&info),
        Some(&mut client.clone()),
        Some(&url),
        Some(&settings),
        None,
        Some(&mut context),
    );
    if accepted == 0 {
        return Err("CEF rejected asynchronous windowless browser creation".into());
    }
    runtime::browser_opened();
    runtime::schedule_work(0);
    web.attach(Rc::new(CefWebView(page.clone())));
    Ok(page)
}

pub(super) fn view(page: Rc<Page>) -> View {
    component(move |_| {
        let page = page.clone();
        let event_page = page.clone();
        let mount_page = page.clone();
        let gpu = page.gpu.clone();
        div()
            .focusable(true)
            .overflow_hidden()
            .on_event(move |event| input(&event_page, &event.event))
            .accessible_label("Web page")
            .on_mount(move |handle| {
                if mount_page.visible.replace(mount_page.visible.get() + 1) == 0 {
                    if let Some(browser) = mount_page.browser.borrow().as_ref() {
                        if let Some(host) = browser.host() {
                            host.was_hidden(0);
                            host.invalidate(PaintElementType::VIEW);
                        }
                    }
                }
                handle.retain(Visible(mount_page.clone()));
            })
            .child(
                gpu_view("", &gpu)
                    .absolute()
                    .left(0.)
                    .top(0.)
                    .w_full()
                    .h_full(),
            )
    })
}

pub(super) fn page_view(web: &WebView) -> View {
    match backend(web) {
        Ok(page) => view(page),
        Err(error) => {
            let message = format!("Chromium could not start: {error}");
            component(move |_| {
                div()
                    .accessible_label("Chromium error")
                    .child(text(message.clone()))
            })
        }
    }
}

struct Visible(Rc<Page>);
impl Drop for Visible {
    fn drop(&mut self) {
        if self.0.visible.get() > 0 && self.0.visible.get() == 1 {
            if let Some(browser) = self.0.browser.borrow().as_ref() {
                if let Some(host) = browser.host() {
                    host.was_hidden(1);
                }
            }
        }
        self.0.visible.set(self.0.visible.get().saturating_sub(1));
    }
}

fn input(page: &Page, event: &InputEvent) {
    let Some(browser) = page.browser.borrow().as_ref().cloned() else {
        return;
    };
    let Some(host) = browser.host() else {
        return;
    };
    match event.clone() {
        InputEvent::PointerDown { x, y, button } | InputEvent::PointerUp { x, y, button } => {
            let mouse_up = matches!(event, InputEvent::PointerUp { .. });
            let button = match button {
                PointerButton::Primary => MouseButtonType::LEFT,
                PointerButton::Secondary => MouseButtonType::RIGHT,
                PointerButton::Middle => MouseButtonType::MIDDLE,
                PointerButton::Other(_) => return,
            };
            host.send_mouse_click_event(
                Some(&MouseEvent {
                    x: x as i32,
                    y: y as i32,
                    modifiers: 0,
                }),
                button,
                mouse_up as i32,
                1,
            );
        }
        InputEvent::PointerMove { x, y } => host.send_mouse_move_event(
            Some(&MouseEvent {
                x: x as i32,
                y: y as i32,
                modifiers: 0,
            }),
            0,
        ),
        InputEvent::PointerLeave => host.send_mouse_move_event(Some(&MouseEvent::default()), 1),
        InputEvent::Scroll {
            x,
            y,
            delta_x,
            delta_y,
        } => host.send_mouse_wheel_event(
            Some(&MouseEvent {
                x: x as i32,
                y: y as i32,
                modifiers: 0,
            }),
            delta_x as i32,
            delta_y as i32,
        ),
        InputEvent::Focus => {
            host.set_focus(1);
        }
        InputEvent::Blur => {
            host.set_focus(0);
        }
        InputEvent::Text(text) | InputEvent::ImeCommit(text) => {
            // KeyEvent uses UTF-16 code units; each committed scalar is sent
            // separately, preserving supplementary characters as surrogate pairs.
            for unit in text.encode_utf16() {
                let event = KeyEvent {
                    type_: KeyEventType::CHAR,
                    windows_key_code: unit as i32,
                    character: unit,
                    unmodified_character: unit,
                    ..Default::default()
                };
                host.send_key_event(Some(&event));
            }
        }
        InputEvent::KeyDown { key, modifiers, .. } => {
            if let Some(code) = cef_key_code(&key) {
                host.send_key_event(Some(&KeyEvent {
                    type_: KeyEventType::RAWKEYDOWN,
                    modifiers: cef_modifiers(modifiers),
                    windows_key_code: code,
                    ..Default::default()
                }));
            }
        }
        InputEvent::KeyUp { key, modifiers } => {
            if let Some(code) = cef_key_code(&key) {
                host.send_key_event(Some(&KeyEvent {
                    type_: KeyEventType::KEYUP,
                    modifiers: cef_modifiers(modifiers),
                    windows_key_code: code,
                    ..Default::default()
                }));
            }
        }
        _ => {}
    }
}

fn cef_key_code(key: &Key) -> Option<i32> {
    Some(match key {
        Key::Tab => 0x09,
        Key::Enter => 0x0d,
        Key::Space => 0x20,
        Key::Escape => 0x1b,
        Key::Backspace => 0x08,
        Key::Delete => 0x2e,
        Key::ArrowLeft => 0x25,
        Key::ArrowUp => 0x26,
        Key::ArrowRight => 0x27,
        Key::ArrowDown => 0x28,
        Key::Home => 0x24,
        Key::End => 0x23,
        Key::PageUp => 0x21,
        Key::PageDown => 0x22,
        Key::Insert => 0x2d,
        Key::ContextMenu => 0x5d,
        Key::Function(n @ 1..=24) => 0x70 + i32::from(*n) - 1,
        Key::Function(_) => return None,
        Key::Character(value) => {
            let mut chars = value.chars();
            let ch = chars.next()?;
            if !ch.is_ascii() || chars.next().is_some() {
                return None;
            }
            i32::from(ch.to_ascii_uppercase() as u8)
        }
    })
}

fn cef_modifiers(modifiers: Modifiers) -> u32 {
    let mut flags = 0;
    if modifiers.shift {
        flags |= cef::sys::cef_event_flags_t::EVENTFLAG_SHIFT_DOWN.0;
    }
    if modifiers.control {
        flags |= cef::sys::cef_event_flags_t::EVENTFLAG_CONTROL_DOWN.0;
    }
    if modifiers.alt {
        flags |= cef::sys::cef_event_flags_t::EVENTFLAG_ALT_DOWN.0;
    }
    if modifiers.meta {
        flags |= cef::sys::cef_event_flags_t::EVENTFLAG_COMMAND_DOWN.0;
    }
    flags
}

fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
}
fn percent_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{cef_key_code, cef_modifiers};
    use zgx::input::{Key, Modifiers};

    #[test]
    fn browser_keys_map_to_chromium_virtual_keys() {
        assert_eq!(cef_key_code(&Key::Character("c".into())), Some(0x43));
        assert_eq!(cef_key_code(&Key::Character("A".into())), Some(0x41));
        assert_eq!(cef_key_code(&Key::ArrowLeft), Some(0x25));
        assert_eq!(cef_key_code(&Key::Enter), Some(0x0d));
        assert_eq!(cef_key_code(&Key::Function(12)), Some(0x7b));
        assert_eq!(cef_key_code(&Key::Function(25)), None);
    }

    #[test]
    fn browser_modifiers_keep_platform_shortcut_keys() {
        let flags = cef_modifiers(Modifiers {
            shift: true,
            control: true,
            alt: true,
            meta: true,
        });
        assert_eq!(flags, 2 | 4 | 8 | 128);
    }
}

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    sync::{Arc, Mutex},
    time::Instant,
};

use cef::*;
use zgx::{
    compose::{View, component, div, gpu_view, text},
    input::{InputEvent, Key, Modifiers, PointerButton},
    style::Styled,
    web_view::{FindOptions, FindResult, WebProfile, WebView, WebViewBackend, WebViewCommand},
};

use super::{
    client::{self, BrowserCallbacks},
    rendering::{Painter, SharedFrame, SharedPopup},
    runtime,
};

pub(super) struct CefWebView(Rc<Page>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BrowserLifecycle {
    Creating,
    Ready,
    Closing,
    Closed,
}

pub(super) struct Page {
    web: zgx::web_view::WeakWebView,
    browser: Rc<RefCell<Option<Browser>>>,
    pending: RefCell<Vec<WebViewCommand>>,
    evaluations: Rc<RefCell<HashMap<i32, (u64, EvaluationReply)>>>,
    navigation_generation: Rc<Cell<u64>>,
    site_data: Rc<RefCell<HashMap<i32, SiteDataReply>>>,
    pending_site_data: RefCell<Vec<(String, SiteDataReply)>>,
    find_replies: client::FindReplies,
    next_evaluation: Cell<i32>,
    devtools_registration: RefCell<Option<Registration>>,
    lifecycle: Cell<BrowserLifecycle>,
    visible: Cell<u32>,
    input_modifiers: Cell<Modifiers>,
    last_click: RefCell<Option<LastClick>>,
    click_count: Cell<u32>,
    gpu: zgx::gpu_view::GpuView,
    frame: SharedFrame,
    popup: SharedPopup,
    size: Arc<Mutex<(i32, i32)>>,
    scale: Arc<Mutex<f32>>,
}

#[derive(Clone, Copy)]
struct LastClick {
    at: Instant,
    x: i32,
    y: i32,
    button: u8,
    count: u32,
}

type EvaluationReply = Box<dyn FnOnce(Result<String, String>)>;
type SiteDataReply = Box<dyn FnOnce(Result<usize, String>)>;

wrap_dev_tools_message_observer! {
    struct CEFScriptObserver {
        evaluations: Rc<RefCell<HashMap<i32, (u64, EvaluationReply)>>>,
        site_data: Rc<RefCell<HashMap<i32, SiteDataReply>>>,
        navigation_generation: Rc<Cell<u64>>,
    }

    impl DevToolsMessageObserver {
        fn on_dev_tools_method_result(
            &self,
            _browser: Option<&mut Browser>,
            message_id: i32,
            success: i32,
            result: Option<&[u8]>,
        ) {
            let evaluation = self.evaluations.borrow_mut().remove(&message_id);
            if let Some((generation, reply)) = evaluation {
                let result = parse_evaluation_for_generation(
                    generation,
                    self.navigation_generation.get(),
                    success,
                    result,
                );
                // CEF can invoke this observer while its DevTools observer
                // list is being traversed. User callbacks may close a window
                // (and drop this registration), so deliver them at ZGX's next
                // safe point instead of re-entering CEF here.
                zgx::native_view::post(move || reply(result));
            } else {
                let site_data = self.site_data.borrow_mut().remove(&message_id);
                if let Some(reply) = site_data {
                    let result = parse_site_data_result(success, result);
                    zgx::native_view::post(move || reply(result));
                }
            }
        }

        fn on_dev_tools_agent_attached(&self, _browser: Option<&mut Browser>) {
        }

        fn on_dev_tools_agent_detached(&self, _browser: Option<&mut Browser>) {
        }
    }
}

fn parse_evaluation_result(success: i32, result: Option<&[u8]>) -> Result<String, String> {
    if success == 0 {
        return Err(result
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .unwrap_or("Chromium rejected the JavaScript evaluation")
            .to_owned());
    }
    let bytes = result.ok_or_else(|| "Chromium returned an empty JavaScript result".to_owned())?;
    let response: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("Chromium returned invalid evaluation JSON: {error}"))?;
    let payload = if response.get("exceptionDetails").is_some() {
        &response
    } else {
        response.get("result").unwrap_or(&response)
    };
    if let Some(exception) = payload.get("exceptionDetails") {
        let message = exception
            .get("exception")
            .and_then(|value| value.get("description"))
            .and_then(serde_json::Value::as_str)
            .or_else(|| exception.get("text").and_then(serde_json::Value::as_str))
            .unwrap_or("JavaScript evaluation threw an exception");
        return Err(message.to_owned());
    }
    let value = payload.get("result").unwrap_or(payload);
    if value.get("type").and_then(serde_json::Value::as_str) == Some("undefined") {
        return Ok("null".to_owned());
    }
    let value = value
        .get("value")
        .ok_or_else(|| "JavaScript result was not serializable by value".to_owned())?;
    serde_json::to_string(value)
        .map_err(|error| format!("cannot encode JavaScript result: {error}"))
}

fn parse_evaluation_for_generation(
    requested_generation: u64,
    current_generation: u64,
    success: i32,
    result: Option<&[u8]>,
) -> Result<String, String> {
    if requested_generation != current_generation {
        return Err("JavaScript evaluation was superseded by a navigation".into());
    }
    parse_evaluation_result(success, result)
}

fn parse_site_data_result(success: i32, result: Option<&[u8]>) -> Result<usize, String> {
    if success == 0 {
        return Err(result
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .unwrap_or("Chromium rejected site-data removal")
            .to_owned());
    }
    let bytes = result.ok_or_else(|| "Chromium returned no site-data result".to_owned())?;
    let response: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("Chromium returned invalid site-data JSON: {error}"))?;
    if let Some(error) = response.get("error") {
        return Err(error.to_string());
    }
    if !response.is_object() {
        return Err("Chromium returned an invalid site-data result".into());
    }
    // CDP confirms the origin was cleared but does not report a record count.
    Ok(1)
}

impl WebViewBackend for CefWebView {
    fn run(&self, command: WebViewCommand) {
        if matches!(
            self.0.lifecycle.get(),
            BrowserLifecycle::Closing | BrowserLifecycle::Closed
        ) {
            if let WebViewCommand::Evaluate {
                reply: Some(reply), ..
            } = command
            {
                reply(Err("CEF browser is closing".into()));
            }
            return;
        }
        if let Some(browser) = self.0.browser.borrow().as_ref() {
            apply(&self.0, browser, command);
        } else {
            self.0.pending.borrow_mut().push(command);
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn find(&self, text: &str, options: &FindOptions, reply: Box<dyn FnOnce(FindResult)>) {
        let Some(browser) = self.0.browser.borrow().as_ref().cloned() else {
            reply(FindResult::default());
            return;
        };
        self.0.find_replies.borrow_mut().push_back(reply);
        let query: CefString = text.into();
        if let Some(host) = browser.host() {
            host.find(
                Some(&query),
                (!options.backwards) as i32,
                options.case_sensitive as i32,
                0,
            );
        } else if let Some(reply) = self.0.find_replies.borrow_mut().pop_back() {
            reply(FindResult::default());
        }
    }

    fn clear_find(&self) {
        if let Some(browser) = self.0.browser.borrow().as_ref()
            && let Some(host) = browser.host()
        {
            host.stop_finding(0);
        }
    }

    fn print(&self) {
        if let Some(browser) = self.0.browser.borrow().as_ref()
            && let Some(host) = browser.host()
        {
            host.print();
        }
    }

    fn clear_site_data(&self, url: &str, done: Box<dyn FnOnce(Result<usize, String>)>) {
        if matches!(
            self.0.lifecycle.get(),
            BrowserLifecycle::Closing | BrowserLifecycle::Closed
        ) {
            done(Err("CEF browser is closing".into()));
            return;
        }
        let Some(browser) = self.0.browser.borrow().as_ref().cloned() else {
            if self.0.lifecycle.get() == BrowserLifecycle::Creating {
                self.0
                    .pending_site_data
                    .borrow_mut()
                    .push((url.to_owned(), done));
            } else {
                done(Err("CEF browser is not available".into()));
            }
            return;
        };
        start_clear_site_data(&self.0, &browser, url, done);
    }
}

impl Drop for CefWebView {
    fn drop(&mut self) {
        self.0.lifecycle.set(BrowserLifecycle::Closing);
        fail_pending(
            &self.0,
            "CEF browser controller was dropped during creation",
        );
        // `close_browser` can synchronously enter on_before_close. Never hold
        // the RefCell borrow across that CEF call: on_before_close clears it.
        close_after_releasing_slot(&self.0.browser, |browser| {
            if let Some(host) = browser.host() {
                host.close_browser(1);
            }
        });
    }
}

fn close_after_releasing_slot<T: Clone>(slot: &RefCell<Option<T>>, close: impl FnOnce(T)) {
    let browser = slot.borrow().as_ref().cloned();
    if let Some(browser) = browser {
        close(browser);
    }
}

fn fail_pending(page: &Page, reason: &str) {
    let commands = std::mem::take(&mut *page.pending.borrow_mut());
    let evaluations = std::mem::take(&mut *page.evaluations.borrow_mut());
    let site_data = std::mem::take(&mut *page.site_data.borrow_mut());
    let pending_site_data = std::mem::take(&mut *page.pending_site_data.borrow_mut());
    let find_replies = std::mem::take(&mut *page.find_replies.borrow_mut());
    if commands.is_empty()
        && evaluations.is_empty()
        && site_data.is_empty()
        && pending_site_data.is_empty()
        && find_replies.is_empty()
    {
        return;
    }
    let reason = reason.to_owned();
    zgx::native_view::post(move || {
        for command in commands {
            if let WebViewCommand::Evaluate {
                reply: Some(reply), ..
            } = command
            {
                reply(Err(reason.clone()));
            }
        }
        for (_, (_, reply)) in evaluations {
            reply(Err(reason.clone()));
        }
        for (_, reply) in site_data {
            reply(Err(reason.clone()));
        }
        for (_, reply) in pending_site_data {
            reply(Err(reason.clone()));
        }
        for reply in find_replies {
            reply(FindResult::default());
        }
    });
}

fn cancel_pending_evaluations(
    evaluations: &Rc<RefCell<HashMap<i32, (u64, EvaluationReply)>>>,
) -> Vec<EvaluationReply> {
    std::mem::take(&mut *evaluations.borrow_mut())
        .into_values()
        .map(|(_, reply)| reply)
        .collect()
}

fn start_clear_site_data(page: &Page, browser: &Browser, url: &str, done: SiteDataReply) {
    let Some(host) = browser.host() else {
        done(Err("CEF browser host is not available".into()));
        return;
    };
    let Some(origin) = super::client::origin_from_url(url)
        .filter(|origin| origin.starts_with("http://") || origin.starts_with("https://"))
    else {
        done(Err(
            "CEF site-data removal requires an HTTP or HTTPS origin".into(),
        ));
        return;
    };
    if page.devtools_registration.borrow().is_none() {
        done(Err("CEF site-data observer is unavailable".into()));
        return;
    }
    let mut id = page.next_evaluation.get().wrapping_add(1).max(1);
    while page.evaluations.borrow().contains_key(&id) || page.site_data.borrow().contains_key(&id) {
        id = id.wrapping_add(1).max(1);
    }
    page.next_evaluation.set(id);
    let Some(mut params) = dictionary_value_create() else {
        done(Err("CEF could not allocate site-data parameters".into()));
        return;
    };
    let origin_key: CefString = "origin".into();
    let origin_value: CefString = origin.as_str().into();
    let types_key: CefString = "storageTypes".into();
    let types_value: CefString = "all".into();
    let _ = params.set_string(Some(&origin_key), Some(&origin_value));
    let _ = params.set_string(Some(&types_key), Some(&types_value));
    page.site_data.borrow_mut().insert(id, done);
    let method: CefString = "Storage.clearDataForOrigin".into();
    if host.execute_dev_tools_method(id, Some(&method), Some(&mut params)) == 0
        && let Some(done) = page.site_data.borrow_mut().remove(&id)
    {
        done(Err("CEF rejected Storage.clearDataForOrigin".into()));
    }
}

fn apply(page: &Page, browser: &Browser, command: WebViewCommand) {
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
            // A single History API navigation is deterministic; repeated
            // GoBack/GoForward calls race CEF's asynchronous history updates.
            if let Some(frame) = browser.main_frame() {
                let script = format!("history.go({n})");
                let script: CefString = script.as_str().into();
                let source: CefString = "zgx://history/go".into();
                frame.execute_java_script(Some(&script), Some(&source), 1);
            }
        }
        WebViewCommand::Evaluate { script, reply } => {
            if let Some(reply) = reply {
                if page.devtools_registration.borrow().is_none() {
                    reply(Err("CEF evaluation observer is unavailable".into()));
                    return;
                }
                let Some(host) = browser.host() else {
                    reply(Err("CEF browser host is unavailable".into()));
                    return;
                };
                let mut id = page.next_evaluation.get().wrapping_add(1).max(1);
                while page.evaluations.borrow().contains_key(&id) {
                    id = id.wrapping_add(1).max(1);
                }
                page.next_evaluation.set(id);
                let Some(mut params) = dictionary_value_create() else {
                    reply(Err("CEF could not allocate evaluation parameters".into()));
                    return;
                };
                let expression: CefString = script.as_str().into();
                let expression_key: CefString = "expression".into();
                let by_value_key: CefString = "returnByValue".into();
                let await_key: CefString = "awaitPromise".into();
                let _ = params.set_string(Some(&expression_key), Some(&expression));
                let _ = params.set_bool(Some(&by_value_key), 1);
                let _ = params.set_bool(Some(&await_key), 1);
                page.evaluations
                    .borrow_mut()
                    .insert(id, (page.navigation_generation.get(), reply));
                let method: CefString = "Runtime.evaluate".into();
                let accepted = host.execute_dev_tools_method(id, Some(&method), Some(&mut params));
                if accepted == 0
                    && let Some((_, reply)) = page.evaluations.borrow_mut().remove(&id)
                {
                    reply(Err("CEF rejected Runtime.evaluate".into()));
                }
            } else if let Some(frame) = browser.main_frame() {
                let script: CefString = script.as_str().into();
                let source: CefString = "polyscale://evaluate".into();
                frame.execute_java_script(Some(&script), Some(&source), 1);
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
    runtime::ensure_accepting_browsers()?;
    let (profile, persistent) = match &web.options().profile {
        WebProfile::Default => ("default".to_owned(), true),
        WebProfile::Ephemeral => (format!("ephemeral:{}", web.id()), false),
        WebProfile::Named(id) => (format!("named:{id}"), true),
    };
    let context = runtime::request_context(&profile, persistent)?;
    let frame = Arc::new(Mutex::default());
    let popup = Arc::new(Mutex::default());
    let size = Arc::new(Mutex::new((1024, 768)));
    let scale = Arc::new(Mutex::new(1.));
    let browser = Rc::new(RefCell::new(None));
    let gpu = zgx_gpu::viewport::view(Painter::new(
        frame.clone(),
        popup.clone(),
        size.clone(),
        scale.clone(),
        browser.clone(),
    ));
    let page = Rc::new(Page {
        web: web.downgrade(),
        browser: browser.clone(),
        pending: RefCell::new(Vec::new()),
        evaluations: Rc::new(RefCell::new(HashMap::new())),
        navigation_generation: Rc::new(Cell::new(0)),
        site_data: Rc::new(RefCell::new(HashMap::new())),
        pending_site_data: RefCell::new(Vec::new()),
        find_replies: Rc::new(RefCell::new(std::collections::VecDeque::new())),
        next_evaluation: Cell::new(0),
        devtools_registration: RefCell::new(None),
        lifecycle: Cell::new(BrowserLifecycle::Creating),
        visible: Cell::new(0),
        input_modifiers: Cell::new(Modifiers::default()),
        last_click: RefCell::new(None),
        click_count: Cell::new(1),
        gpu,
        frame,
        popup,
        size,
        scale,
    });
    let weak = Rc::downgrade(&page);
    let created_weak = weak.clone();
    let closed_weak = weak;
    let navigation_weak = Rc::downgrade(&page);
    let callbacks = BrowserCallbacks {
        find_replies: page.find_replies.clone(),
        web: web.downgrade(),
        new_windows: web.options().new_windows,
        navigation_started: Rc::new(move || {
            let Some(page) = navigation_weak.upgrade() else {
                return;
            };
            page.navigation_generation
                .set(page.navigation_generation.get().wrapping_add(1));
            let replies = cancel_pending_evaluations(&page.evaluations);
            if !replies.is_empty() {
                zgx::native_view::post(move || {
                    for reply in replies {
                        reply(Err(
                            "JavaScript evaluation was superseded by a navigation".into()
                        ));
                    }
                });
            }
        }),
        created: Rc::new(move |browser| {
            runtime::browser_created(browser.identifier());
            if runtime::register_browser(&browser).is_err() {
                if let Some(host) = browser.host() {
                    host.close_browser(1);
                }
                return;
            }
            let Some(page) = created_weak.upgrade() else {
                if let Some(host) = browser.host() {
                    host.close_browser(1);
                }
                return;
            };
            *page.browser.borrow_mut() = Some(browser.clone());
            if page.lifecycle.get() != BrowserLifecycle::Creating {
                page.lifecycle.set(BrowserLifecycle::Closing);
                if let Some(host) = browser.host() {
                    host.close_browser(1);
                }
                return;
            }
            page.lifecycle.set(BrowserLifecycle::Ready);
            if let Some(host) = browser.host() {
                let mut observer = CEFScriptObserver::new(
                    page.evaluations.clone(),
                    page.site_data.clone(),
                    page.navigation_generation.clone(),
                );
                page.devtools_registration
                    .replace(host.add_dev_tools_message_observer(Some(&mut observer)));
                host.was_hidden((page.visible.get() == 0) as i32);
                host.invalidate(PaintElementType::VIEW);
            }
            let pending_site_data = std::mem::take(&mut *page.pending_site_data.borrow_mut());
            for (url, reply) in pending_site_data {
                start_clear_site_data(&page, &browser, &url, reply);
            }
            let pending = std::mem::take(&mut *page.pending.borrow_mut());
            for command in pending {
                apply(&page, &browser, command);
            }
            runtime::schedule_work(0);
            let _ = page.web.upgrade();
        }),
        before_close: Rc::new(move |identifier| {
            runtime::browser_closed(identifier);
            if let Some(page) = closed_weak.upgrade() {
                page.lifecycle.set(BrowserLifecycle::Closed);
                fail_pending(&page, "CEF browser closed before the operation completed");
                page.browser.borrow_mut().take();
            }
        }),
    };
    let invalidator = page.gpu.clone();
    let client = client::client(
        page.frame.clone(),
        page.popup.clone(),
        page.size.clone(),
        page.scale.clone(),
        Rc::new(move || invalidator.invalidate()),
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
    let mut extra_info = super::bridge::extra_info(web.options());
    runtime::browser_creation_started();
    let accepted = browser_host_create_browser(
        Some(&info),
        Some(&mut client.clone()),
        Some(&url),
        Some(&settings),
        extra_info.as_mut(),
        Some(&mut context),
    );
    if accepted == 0 {
        runtime::browser_creation_failed();
        return Err("CEF rejected asynchronous windowless browser creation".into());
    }
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
                if mount_page.visible.replace(mount_page.visible.get() + 1) == 0
                    && let Some(browser) = mount_page.browser.borrow().as_ref()
                    && let Some(host) = browser.host()
                {
                    host.was_hidden(0);
                    host.invalidate(PaintElementType::VIEW);
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
        if self.0.visible.get() > 0
            && self.0.visible.get() == 1
            && let Some(browser) = self.0.browser.borrow().as_ref()
            && let Some(host) = browser.host()
        {
            host.was_hidden(1);
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
            let (button_type, button_id) = match button {
                PointerButton::Primary => (MouseButtonType::LEFT, 1),
                PointerButton::Secondary => (MouseButtonType::RIGHT, 2),
                PointerButton::Middle => (MouseButtonType::MIDDLE, 3),
                PointerButton::Other(_) => return,
            };
            let click_count = if mouse_up {
                page.click_count.get()
            } else {
                let count = next_click_count(page, x as i32, y as i32, button_id);
                page.click_count.set(count);
                count
            };
            host.send_mouse_click_event(
                Some(&mouse_event(page, x, y)),
                button_type,
                mouse_up as i32,
                click_count as i32,
            );
        }
        InputEvent::PointerMove { x, y } => {
            host.send_mouse_move_event(Some(&mouse_event(page, x, y)), 0)
        }
        InputEvent::PointerLeave => host.send_mouse_move_event(Some(&MouseEvent::default()), 1),
        InputEvent::Scroll {
            x,
            y,
            delta_x,
            delta_y,
        } => host.send_mouse_wheel_event(
            Some(&mouse_event(page, x, y)),
            delta_x as i32,
            delta_y as i32,
        ),
        InputEvent::Focus => {
            host.set_focus(1);
        }
        InputEvent::Blur => {
            page.input_modifiers.set(Modifiers::default());
            host.set_focus(0);
        }
        InputEvent::Text(text) => send_text(&host, &text),
        InputEvent::ImePreedit { text, cursor } => {
            let source = text;
            let text: CefString = source.as_str().into();
            let total = source.encode_utf16().count() as u32;
            let selection = cursor
                .map(|(start, end)| {
                    let start = utf16_offset(&source, start);
                    let end = utf16_offset(&source, end);
                    Range {
                        from: start.min(end),
                        to: start.max(end),
                    }
                })
                .unwrap_or(Range {
                    from: total,
                    to: total,
                });
            host.ime_set_composition(Some(&text), None, None, Some(&selection));
        }
        InputEvent::ImeCommit(text) => {
            // OSR's platform IME adapters can deliver a committed string even
            // when Chromium did not retain a composition node (for example,
            // dead-key and emoji-picker commits on macOS). Cancel any visual
            // preedit, then deliver the already-committed Unicode through the
            // same CHAR path as ordinary text input.
            host.ime_cancel_composition();
            send_text(&host, &text);
        }
        InputEvent::KeyDown { key, modifiers, .. } => {
            page.input_modifiers.set(modifiers);
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
            page.input_modifiers.set(modifiers);
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

fn send_text(host: &BrowserHost, text: &str) {
    for event in text_key_events(text) {
        host.send_key_event(Some(&event));
    }
}

fn text_key_events(text: &str) -> impl Iterator<Item = KeyEvent> + '_ {
    // KeyEvent uses UTF-16 code units; supplementary characters are a pair.
    text.encode_utf16().map(|unit| KeyEvent {
        type_: KeyEventType::CHAR,
        windows_key_code: unit as i32,
        character: unit,
        unmodified_character: unit,
        ..Default::default()
    })
}

fn mouse_event(page: &Page, x: f32, y: f32) -> MouseEvent {
    MouseEvent {
        x: x as i32,
        y: y as i32,
        modifiers: cef_modifiers(page.input_modifiers.get()),
    }
}

fn next_click_count(page: &Page, x: i32, y: i32, button: u8) -> u32 {
    let mut last = page.last_click.borrow_mut();
    click_count(&mut last, Instant::now(), x, y, button)
}

fn click_count(last: &mut Option<LastClick>, now: Instant, x: i32, y: i32, button: u8) -> u32 {
    let count = last
        .filter(|last| {
            now.duration_since(last.at).as_millis() <= 500
                && (x - last.x).abs() <= 4
                && (y - last.y).abs() <= 4
                && button == last.button
        })
        .map(|last| last.count.saturating_add(1).min(3))
        .unwrap_or(1);
    *last = Some(LastClick {
        at: now,
        x,
        y,
        button,
        count,
    });
    count
}

fn utf16_offset(text: &str, byte_offset: usize) -> u32 {
    let byte_offset = byte_offset.min(text.len());
    let mut boundary = byte_offset;
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text[..boundary].encode_utf16().count() as u32
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
mod evaluation_tests {
    use super::{
        EvaluationReply, cancel_pending_evaluations, parse_evaluation_for_generation,
        parse_evaluation_result, parse_site_data_result,
    };
    use std::{cell::RefCell, collections::HashMap, rc::Rc};

    #[test]
    fn devtools_values_are_returned_as_json() {
        for (payload, expected) in [
            (r#"{"result":{"result":{"type":"number","value":3}}}"#, "3"),
            (r#"{"result":{"type":"number","value":3}}"#, "3"),
            (
                r#"{"result":{"result":{"type":"string","value":"hello"}}}"#,
                r#""hello""#,
            ),
            (
                r#"{"result":{"result":{"type":"object","value":{"name":"polyscale","value":42}}}}"#,
                r#"{"name":"polyscale","value":42}"#,
            ),
            (r#"{"result":{"result":{"type":"undefined"}}}"#, "null"),
        ] {
            assert_eq!(
                parse_evaluation_result(1, Some(payload.as_bytes()))
                    .unwrap_or_else(|error| panic!("{payload}: {error}")),
                expected
            );
        }
    }

    #[test]
    fn devtools_javascript_exceptions_are_errors() {
        let response = r#"{"result":{"exceptionDetails":{"text":"Uncaught Error: fixture"}}}"#;
        assert!(
            parse_evaluation_result(1, Some(response.as_bytes()))
                .unwrap_err()
                .contains("fixture")
        );
        let cef_response = r#"{"result":{"type":"object","subtype":"error","description":"Error: fixture"},"exceptionDetails":{"text":"Uncaught","exception":{"description":"Error: fixture"}}}"#;
        assert!(
            parse_evaluation_result(1, Some(cef_response.as_bytes()))
                .unwrap_err()
                .contains("fixture")
        );
    }

    #[test]
    fn navigation_invalidates_results_from_the_previous_document() {
        assert_eq!(
            parse_evaluation_for_generation(
                3,
                4,
                1,
                Some(br#"{"result":{"type":"number","value":42}}"#),
            ),
            Err("JavaScript evaluation was superseded by a navigation".into())
        );
        assert_eq!(
            parse_evaluation_for_generation(
                4,
                4,
                1,
                Some(br#"{"result":{"type":"number","value":42}}"#),
            ),
            Ok("42".into())
        );
    }

    #[test]
    fn navigation_cancels_all_pending_evaluations_once() {
        let seen = Rc::new(RefCell::new(Vec::new()));
        let mut evaluations: HashMap<i32, (u64, EvaluationReply)> = HashMap::new();
        for id in [7, 8] {
            let seen = seen.clone();
            evaluations.insert(
                id,
                (2, Box::new(move |result| seen.borrow_mut().push(result))),
            );
        }
        let evaluations = Rc::new(RefCell::new(evaluations));
        let replies = cancel_pending_evaluations(&evaluations);
        assert!(evaluations.borrow().is_empty());
        assert_eq!(replies.len(), 2);
        for reply in replies {
            reply(Err(
                "JavaScript evaluation was superseded by a navigation".into()
            ));
        }
        assert_eq!(seen.borrow().len(), 2);
        assert!(seen.borrow().iter().all(|result| result.is_err()));
    }

    #[test]
    fn targeted_site_data_result_requires_a_confirmed_cdp_success() {
        assert_eq!(parse_site_data_result(1, Some(br#"{}"#)), Ok(1));
        assert!(parse_site_data_result(0, None).is_err());
        assert!(parse_site_data_result(1, Some(br#"{"error":{"code":-1}}"#)).is_err());
        assert!(parse_site_data_result(1, Some(b"not-json")).is_err());
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use std::time::{Duration, Instant};

    use super::{
        cef_key_code, cef_modifiers, click_count, close_after_releasing_slot, text_key_events,
        utf16_offset,
    };
    use cef::KeyEventType;
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

    #[test]
    fn ime_cursor_byte_offsets_become_utf16_ranges() {
        assert_eq!(utf16_offset("hello", 3), 3);
        assert_eq!(utf16_offset("a🦀b", 5), 3);
        assert_eq!(utf16_offset("a🦀b", 2), 1);
        assert_eq!(utf16_offset("a🦀b", 99), 4);
    }

    #[test]
    fn committed_ime_text_uses_utf16_char_events() {
        let events: Vec<_> = text_key_events("a🦀").collect();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].character, b'a' as u16);
        assert_eq!(events[1].character, 0xd83e);
        assert_eq!(events[2].character, 0xdd80);
        assert!(events.iter().all(|event| event.type_ == KeyEventType::CHAR));
    }

    #[test]
    fn mouse_click_count_tracks_double_and_triple_clicks() {
        let mut previous = None;
        let now = Instant::now();
        assert_eq!(click_count(&mut previous, now, 10, 10, 1), 1);
        assert_eq!(
            click_count(&mut previous, now + Duration::from_millis(120), 11, 10, 1),
            2
        );
        assert_eq!(
            click_count(&mut previous, now + Duration::from_millis(220), 11, 10, 1),
            3
        );
        assert_eq!(
            click_count(&mut previous, now + Duration::from_millis(300), 11, 10, 2),
            1
        );
        assert_eq!(
            click_count(&mut previous, now + Duration::from_millis(900), 11, 10, 2),
            1
        );
    }

    #[test]
    fn synchronous_close_callback_can_clear_the_browser_slot() {
        let slot = RefCell::new(Some(Rc::new(())));
        close_after_releasing_slot(&slot, |_| {
            slot.borrow_mut().take();
        });
        assert!(slot.borrow().is_none());
    }
}

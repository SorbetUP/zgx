use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    rc::{Rc, Weak},
    sync::{Arc, Mutex},
    time::Instant,
};

use cef::*;
use zgx::{
    compose::{View, component, div, gpu_view, text},
    input::{DragPhase, DraggedText, InputEvent, Key, Modifiers, PointerButton},
    scene::{NodeId, Scene},
    style::Styled,
    web_view::{FindOptions, FindResult, WebProfile, WebView, WebViewBackend, WebViewCommand},
};

use super::{
    client::{self, BrowserCallbacks},
    rendering::{Painter, SharedFrame, SharedPopup, release_frame_buffers},
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
    /// The exact asynchronous CEF creation request owned by this page. It is
    /// cleared when the browser callback arrives or the page is dropped.
    creation_id: Cell<Option<u64>>,
    permissions: runtime::PermissionStoreHandle,
    origins: runtime::OriginStoreHandle,
    browser: Rc<RefCell<Option<Browser>>>,
    html_documents: client::HtmlDocuments,
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
    mouse_buttons: Cell<u32>,
    drag_target_data: RefCell<Option<DragData>>,
    drag_target_files: RefCell<HashSet<std::path::PathBuf>>,
    drag_source_pending: RefCell<Option<PageDrag>>,
    drag_export: RefCell<Option<Vec<zgx::input::DragItem>>>,
    drag_source_active: Cell<bool>,
    pressed: Cell<Option<(f32, f32)>>,
    node: Cell<Option<NodeId>>,
    scene: RefCell<Weak<RefCell<Scene>>>,
    last_click: RefCell<Option<LastClick>>,
    click_count: Cell<u32>,
    gpu: zgx::gpu_view::GpuView,
    frame: SharedFrame,
    popup: SharedPopup,
    size: Arc<Mutex<(i32, i32)>>,
    scale: Arc<Mutex<f32>>,
}

struct PageDrag {
    text: String,
    html: Option<String>,
    link_url: Option<String>,
    files: Vec<std::path::PathBuf>,
    items: Vec<zgx::input::DragItem>,
    x: i32,
    y: i32,
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
        super::cef_trace!("WEBVIEW_BACKEND_DROP");
        self.0.lifecycle.set(BrowserLifecycle::Closing);
        if let Some(creation_id) = self.0.creation_id.take() {
            runtime::browser_creation_cancelled(creation_id);
        }
        fail_pending(
            &self.0,
            "CEF browser controller was dropped during creation",
        );
        // `close_browser` can synchronously enter on_before_close. Never hold
        // the RefCell borrow across that CEF call: on_before_close clears it.
        close_after_releasing_slot(&self.0.browser, |browser| {
            if let Some(host) = browser.host() {
                super::cef_trace!(&format!(
                    "CLOSE_REQUEST backend_drop browser={}",
                    browser.identifier()
                ));
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
    // Clear every origin of the site, not just the one asked for: the URL's
    // own origin plus every origin this profile knows — persisted from
    // earlier runs and this session — whose host's registrable domain
    // (eTLD+1, from the Public Suffix List) is the URL's, so sibling
    // subdomains (a.b.example.co.uk next to example.co.uk) and other ports
    // of the site go too.
    let Some(site_host) = zgx::web_view::site_host(url) else {
        done(Err(format!("{url} has no site")));
        return;
    };
    let Some(site) = runtime::registrable_domain(&site_host) else {
        done(Err(format!("{url} has no registrable domain")));
        return;
    };
    let origins = runtime::site_origins(&page.origins, &site, &origin);
    // Cookies are the one record a per-origin storage clear can miss: a
    // cookie set with `Domain=.example.com` belongs to no single origin, and
    // HttpOnly ones never reach script. Enumerate them all through the
    // profile's cookie manager and delete every one whose domain belongs to
    // the site, then ask delete_cookies to remove each by URL and name as
    // well.
    let outcome = Rc::new(RefCell::new((0usize, None::<String>)));
    match delete_site_cookies(&host, &site) {
        Ok(count) => outcome.borrow_mut().0 += count,
        Err(error) => {
            let mut outcome = outcome.borrow_mut();
            if outcome.1.is_none() {
                outcome.1 = Some(error);
            }
        }
    }
    let done = Rc::new(RefCell::new(Some(done)));
    let remaining_slot = Rc::new(std::cell::Cell::new(origins.len()));
    let store = page.permissions.clone();
    for cleared_origin in origins {
        let mut id = page.next_evaluation.get().wrapping_add(1).max(1);
        while page.evaluations.borrow().contains_key(&id)
            || page.site_data.borrow().contains_key(&id)
        {
            id = id.wrapping_add(1).max(1);
        }
        page.next_evaluation.set(id);
        let Some(mut params) = dictionary_value_create() else {
            if let Some(done) = done.borrow_mut().take() {
                done(Err("CEF could not allocate site-data parameters".into()));
            }
            return;
        };
        let origin_key: CefString = "origin".into();
        let origin_value: CefString = cleared_origin.as_str().into();
        let types_key: CefString = "storageTypes".into();
        let types_value: CefString = "all".into();
        let _ = params.set_string(Some(&origin_key), Some(&origin_value));
        let _ = params.set_string(Some(&types_key), Some(&types_value));
        let remaining_slot = remaining_slot.clone();
        let outcome = outcome.clone();
        let done = done.clone();
        let store = store.clone();
        let origin_for_reply = cleared_origin.clone();
        let reply: SiteDataReply = Box::new(move |result| {
            {
                let mut outcome = outcome.borrow_mut();
                match result {
                    Ok(count) => {
                        outcome.0 += count;
                        if let Err(error) = store.borrow_mut().forget_origin(&origin_for_reply)
                            && outcome.1.is_none()
                        {
                            outcome.1 = Some(format!(
                                "site data was cleared but its saved permissions could not be removed: {error}"
                            ));
                        }
                    }
                    Err(error) => {
                        if outcome.1.is_none() {
                            outcome.1 = Some(error);
                        }
                    }
                }
            }
            let left = remaining_slot.get() - 1;
            remaining_slot.set(left);
            if left == 0
                && let Some(done) = done.borrow_mut().take()
            {
                let (count, error) = (outcome.borrow().0, outcome.borrow().1.clone());
                match error {
                    Some(error) => done(Err(error)),
                    None => done(Ok(count)),
                }
            }
        });
        page.site_data.borrow_mut().insert(id, reply);
        let method: CefString = "Storage.clearDataForOrigin".into();
        if host.execute_dev_tools_method(id, Some(&method), Some(&mut params)) == 0
            && let Some(reply) = page.site_data.borrow_mut().remove(&id)
        {
            reply(Err("CEF rejected Storage.clearDataForOrigin".into()));
        }
    }
}

wrap_cookie_visitor! {
    struct SiteCookieVisitor {
        site: String,
        matched: Rc<RefCell<Vec<(String, String, String)>>>,
        deleted: Rc<std::cell::Cell<usize>>,
    }
    impl CookieVisitor {
        fn visit(
            &self,
            cookie: Option<&Cookie>,
            _count: i32,
            _total: i32,
            delete_cookie: Option<&mut i32>,
        ) -> i32 {
            let Some(cookie) = cookie else { return 0 };
            let domain = cookie.domain.to_string();
            let Some(registrable) = runtime::registrable_domain(domain.trim_start_matches('.'))
            else {
                return 0;
            };
            if registrable == self.site {
                // Delete the cookie right here, HttpOnly ones included, and
                // remember it so delete_cookies removes it by URL and name
                // as well.
                if let Some(delete) = delete_cookie {
                    *delete = 1;
                }
                self.deleted.set(self.deleted.get() + 1);
                self.matched.borrow_mut().push((
                    cookie.name.to_string(),
                    domain,
                    cookie.path.to_string(),
                ));
            }
            0
        }
    }
}

/// Delete every cookie of the profile's request context whose domain belongs
/// to `site` (the registrable domain being cleared): host-only cookies of the
/// site's origins, `Domain=.example.com` ones shared by its subdomains, and
/// HttpOnly ones. Returns how many cookies matched.
fn delete_site_cookies(host: &BrowserHost, site: &str) -> Result<usize, String> {
    let context = host
        .request_context()
        .ok_or_else(|| "CEF request context is not available".to_owned())?;
    let manager = context
        .cookie_manager(None)
        .ok_or_else(|| "CEF cookie manager is not available".to_owned())?;
    let matched: Rc<RefCell<Vec<(String, String, String)>>> = Rc::new(RefCell::new(Vec::new()));
    let deleted = Rc::new(std::cell::Cell::new(0usize));
    let mut visitor = SiteCookieVisitor::new(site.to_owned(), matched.clone(), deleted.clone());
    manager.visit_all_cookies(Some(&mut visitor));
    for (name, domain, path) in matched.borrow().iter() {
        let host_part = domain.trim_start_matches('.');
        let path = if path.is_empty() { "/" } else { path.as_str() };
        for scheme in ["http", "https"] {
            let url: CefString = format!("{scheme}://{host_part}{path}").as_str().into();
            let name: CefString = name.as_str().into();
            let _ = manager.delete_cookies(Some(&url), Some(&name), None);
        }
    }
    Ok(deleted.get())
}

fn apply(page: &Page, browser: &Browser, command: WebViewCommand) {
    match command {
        WebViewCommand::LoadUrl(url) => {
            page.note_navigation();
            page.html_documents
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clear();
            let frame = browser.main_frame();
            if let Some(frame) = frame {
                frame.load_url(Some(&url.as_str().into()));
            }
        }
        WebViewCommand::LoadHtml { html, base_url } => {
            page.note_navigation();
            let frame = browser.main_frame();
            if let Some(base) = base_url
                && let Some(document_url) = client::normalize_document_url(&base)
            {
                {
                    let mut documents = page
                        .html_documents
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    documents.clear();
                    documents.insert(document_url, Arc::new(html.into_bytes()));
                }
                if let Some(frame) = frame {
                    frame.load_url(Some(&base.as_str().into()));
                }
            } else {
                page.html_documents
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clear();
                let url = format!("data:text/html;charset=utf-8,{}", percent_encode(&html));
                if let Some(frame) = frame {
                    frame.load_url(Some(&url.as_str().into()));
                }
            }
        }
        WebViewCommand::Reload => {
            page.note_navigation();
            browser.reload();
        }
        WebViewCommand::Stop => browser.stop_load(),
        WebViewCommand::GoBack => {
            page.note_navigation();
            browser.go_back();
        }
        WebViewCommand::GoForward => {
            page.note_navigation();
            browser.go_forward();
        }
        WebViewCommand::GoToHistory(n) => {
            page.note_navigation();
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
            // CEF OSR needs a separate host surface for the DevTools browser.
            // ZGX has no inspector window attached to this page yet, so make
            // the unsupported command visible instead of silently pretending
            // that an inspector opened.
            eprintln!("CEF DevTools are unavailable: no inspector host surface is attached");
        }
        WebViewCommand::SetZoom(zoom) => {
            if let Some(host) = browser.host() {
                host.set_zoom_level(zoom.log2());
            }
        }
    }
}

pub(super) fn backend(
    web: &WebView,
    dialogs: crate::FileDialogs,
    tasks: crate::TaskSpawner,
    windows: crate::WindowFactory,
) -> Result<Rc<Page>, String> {
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
    let permissions = runtime::permission_store(&profile, persistent)?;
    let origins = runtime::origin_store(&profile, persistent)?;
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
        creation_id: Cell::new(None),
        permissions: permissions.clone(),
        origins: origins.clone(),
        browser: browser.clone(),
        html_documents: Arc::new(Mutex::new(HashMap::new())),
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
        mouse_buttons: Cell::new(0),
        drag_target_data: RefCell::new(None),
        drag_target_files: RefCell::new(HashSet::new()),
        drag_source_pending: RefCell::new(None),
        drag_export: RefCell::new(None),
        drag_source_active: Cell::new(false),
        pressed: Cell::new(None),
        node: Cell::new(None),
        scene: RefCell::new(Weak::new()),
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
    let closed_frame = page.frame.clone();
    let closed_popup = page.popup.clone();
    let navigation_weak = Rc::downgrade(&page);
    let creation_id = runtime::browser_creation_started();
    page.creation_id.set(Some(creation_id));
    let callbacks = BrowserCallbacks {
        find_replies: page.find_replies.clone(),
        web: web.downgrade(),
        new_windows: web.options().new_windows,
        navigation_started: Rc::new(move || {
            if let Some(page) = navigation_weak.upgrade() {
                page.note_navigation();
            }
        }),
        created: Rc::new(move |browser| {
            if let Some(page) = created_weak.upgrade()
                && page.creation_id.get() == Some(creation_id)
            {
                page.creation_id.set(None);
            }
            if !runtime::browser_created(creation_id, browser.identifier()) {
                runtime::defer_browser_close(browser.clone(), "late_create");
                return;
            }
            if runtime::register_browser(&browser).is_err() {
                runtime::defer_browser_close(browser.clone(), "register_failed");
                return;
            }
            let Some(page) = created_weak.upgrade() else {
                runtime::defer_browser_close(browser.clone(), "controller_gone");
                return;
            };
            *page.browser.borrow_mut() = Some(browser.clone());
            if page.lifecycle.get() != BrowserLifecycle::Creating {
                super::cef_trace!(&format!(
                    "CLOSE_REQUEST lifecycle={:?} browser={}",
                    page.lifecycle.get(),
                    browser.identifier()
                ));
                page.lifecycle.set(BrowserLifecycle::Closing);
                runtime::defer_browser_close(browser.clone(), "lifecycle_mismatch");
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
            super::cef_trace!(&format!("BEFORE_CLOSE_CALLBACK browser={identifier}"));
            runtime::browser_closed(identifier);
            release_frame_buffers(&closed_frame, &closed_popup);
            if let Some(page) = closed_weak.upgrade() {
                page.lifecycle.set(BrowserLifecycle::Closed);
                fail_pending(&page, "CEF browser closed before the operation completed");
                page.browser.borrow_mut().take();
            }
        }),
    };
    let invalidator = page.gpu.clone();
    let drag_start_page = Rc::downgrade(&page);
    let drag_start = Rc::new(move |data: &DragData, x: i32, y: i32| -> bool {
        if let Some(page) = drag_start_page.upgrade()
            && let Some(drag) = page_drag(data, x, y)
        {
            page.drag_source_pending.replace(Some(drag));
            return true;
        }
        false
    });
    let client = client::client(
        page.frame.clone(),
        page.popup.clone(),
        page.size.clone(),
        page.scale.clone(),
        Rc::new(move || invalidator.invalidate()),
        callbacks,
        web.downgrade(),
        None,
        profile.clone(),
        permissions,
        origins,
        dialogs,
        tasks,
        windows,
        drag_start,
        page.html_documents.clone(),
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
    let accepted = browser_host_create_browser(
        Some(&info),
        Some(&mut client.clone()),
        Some(&url),
        Some(&settings),
        extra_info.as_mut(),
        Some(&mut context),
    );
    if accepted == 0 {
        super::cef_trace!(&format!("CEF_CREATE_REJECTED id={creation_id}"));
        runtime::browser_creation_failed(creation_id);
        page.creation_id.set(None);
        return Err("CEF rejected asynchronous windowless browser creation".into());
    }
    super::cef_trace!(&format!("CEF_CREATE_ACCEPTED id={creation_id}"));
    runtime::schedule_work(0);
    web.attach(Rc::new(CefWebView(page.clone())));
    if runtime::close_race_fixture_enabled() {
        super::cef_trace!("FIXTURE_PAGE_CLOSE_AFTER_ACCEPTED");
        web.close();
    }
    Ok(page)
}

pub(super) struct PreparedPopup {
    pub client: Client,
    pub window_info: WindowInfo,
    pub window: crate::WindowHandle,
}

pub(super) fn prepare_popup(
    web: zgx::web_view::WeakWebView,
    profile: String,
    permissions: runtime::PermissionStoreHandle,
    origins: runtime::OriginStoreHandle,
    dialogs: crate::FileDialogs,
    tasks: crate::TaskSpawner,
    windows: crate::WindowFactory,
    popup_id: i32,
    creation_id: u64,
    popup_url: String,
    pending_popups: Rc<RefCell<HashMap<i32, client::PopupPending>>>,
) -> Option<PreparedPopup> {
    runtime::ensure_accepting_browsers().ok()?;
    // The popup gets its own controller — same reactive runtime as the opener,
    // options copied from it — so its title, URL, messages and evaluations
    // never touch the parent's state. The runtime keeps the strong reference
    // (the Page only holds a weak one, avoiding a reference cycle) until the
    // child browser closes.
    let child = web
        .upgrade()
        .map(|parent| parent.new_related(parent.options().clone()));
    if let Some(child) = &child {
        runtime::retain_child_web(creation_id, child.clone());
    }
    let child_weak = child.as_ref().map(|child| child.downgrade());
    // The child inherits the opener's navigation policy (a policy-less parent
    // allows everything, so this is just delegation).
    if let (Some(parent), Some(child)) = (web.upgrade(), child.as_ref()) {
        let parent_weak = parent.downgrade();
        child.set_navigation_policy(move |request| {
            parent_weak
                .upgrade()
                .is_some_and(|parent| parent.allows(request))
        });
    }
    let page_web = child_weak.clone().unwrap_or_else(|| web.clone());
    let frame = Arc::new(Mutex::default());
    let popup = Arc::new(Mutex::default());
    let size = Arc::new(Mutex::new((800, 640)));
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
        web: page_web.clone(),
        creation_id: Cell::new(Some(creation_id)),
        permissions: permissions.clone(),
        origins: origins.clone(),
        browser: browser.clone(),
        html_documents: Arc::new(Mutex::new(HashMap::new())),
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
        mouse_buttons: Cell::new(0),
        drag_target_data: RefCell::new(None),
        drag_target_files: RefCell::new(HashSet::new()),
        drag_source_pending: RefCell::new(None),
        drag_export: RefCell::new(None),
        drag_source_active: Cell::new(false),
        pressed: Cell::new(None),
        node: Cell::new(None),
        scene: RefCell::new(Weak::new()),
        last_click: RefCell::new(None),
        click_count: Cell::new(1),
        gpu,
        frame,
        popup,
        size,
        scale,
    });
    let handle_slot = Rc::new(RefCell::new(None::<crate::WindowHandle>));
    let created_page = Rc::downgrade(&page);
    let closed_page = Rc::downgrade(&page);
    let popup_opened = web.clone();
    let popup_closed = web.clone();
    let closed_handle = handle_slot.clone();
    let created_map = pending_popups.clone();
    let created_child = child_weak.clone();
    let closed_creation = creation_id;
    let diagnostic_url = popup_url;
    let callbacks = BrowserCallbacks {
        find_replies: page.find_replies.clone(),
        web: web.clone(),
        new_windows: web.upgrade().is_some_and(|web| web.options().new_windows),
        navigation_started: Rc::new({
            let weak = Rc::downgrade(&page);
            move || {
                if let Some(page) = weak.upgrade() {
                    page.note_navigation();
                }
            }
        }),
        created: Rc::new(move |browser| {
            let current_url = browser
                .main_frame()
                .map(|frame| CefString::from(&frame.url()).to_string())
                .unwrap_or_default();
            client::trace_popup_fixture(&format!(
                "CEF_POPUP_CREATED:{}:target={diagnostic_url}:current={current_url}",
                browser.identifier()
            ));
            if let Some(page) = created_page.upgrade()
                && page.creation_id.get() == Some(creation_id)
            {
                page.creation_id.set(None);
            }
            if !runtime::browser_created(creation_id, browser.identifier()) {
                runtime::defer_browser_close(browser.clone(), "popup_late_create");
                return;
            }
            if runtime::register_browser(&browser).is_err() {
                runtime::defer_browser_close(browser.clone(), "popup_register_failed");
                return;
            }
            let Some(page) = created_page.upgrade() else {
                runtime::defer_browser_close(browser.clone(), "popup_controller_gone");
                return;
            };
            *page.browser.borrow_mut() = Some(browser.clone());
            created_map.borrow_mut().remove(&popup_id);
            if page.lifecycle.get() != BrowserLifecycle::Creating {
                page.lifecycle.set(BrowserLifecycle::Closing);
                runtime::defer_browser_close(browser.clone(), "popup_lifecycle_mismatch");
                return;
            }
            page.lifecycle.set(BrowserLifecycle::Ready);
            // Attach the child controller to the browser CEF just made, so
            // commands and events route to it instead of the parent.
            if let Some(child) = created_child.as_ref().and_then(|weak| weak.upgrade()) {
                child.attach(Rc::new(CefWebView(page.clone())));
            }
            client::emit_native_activity(
                &popup_opened,
                format!("popup:{}", browser.identifier()),
                true,
            );
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
        }),
        before_close: Rc::new(move |identifier| {
            client::trace_popup_fixture(&format!("CEF_POPUP_CLOSED:{identifier}"));
            client::emit_native_activity(&popup_closed, format!("popup:{identifier}"), false);
            runtime::release_child_web(closed_creation);
            runtime::browser_closed(identifier);
            if let Some(page) = closed_page.upgrade() {
                page.lifecycle.set(BrowserLifecycle::Closed);
                fail_pending(&page, "CEF popup closed before the operation completed");
                page.browser.borrow_mut().take();
            }
            if let Some(window) = closed_handle.borrow().as_ref() {
                window.close();
            }
        }),
    };
    let invalidator = page.gpu.clone();
    let drag_start_page = Rc::downgrade(&page);
    let drag_start = Rc::new(move |data: &DragData, x: i32, y: i32| -> bool {
        if let Some(page) = drag_start_page.upgrade()
            && let Some(drag) = page_drag(data, x, y)
        {
            page.drag_source_pending.replace(Some(drag));
            return true;
        }
        false
    });
    let client = client::client(
        page.frame.clone(),
        page.popup.clone(),
        page.size.clone(),
        page.scale.clone(),
        Rc::new(move || invalidator.invalidate()),
        callbacks,
        page_web,
        Some(web),
        profile,
        permissions,
        origins,
        dialogs,
        tasks,
        windows.clone(),
        drag_start,
        page.html_documents.clone(),
    );
    let content = view(page.clone());
    // Automated CEF fixtures keep the parent window off-screen so they do not
    // disturb the user's desktop. Keep popup windows off-screen as well; a
    // popup test should exercise the real child browser without displaying a
    // sign-in window on the machine running the test.
    let popup_bounds = std::env::var_os("ZGX_CEF_POPUP_RESULT")
        .map(|_| crate::WindowBounds::new(800, 640).at(-10_000, -10_000));
    let window = windows.open(
        crate::WindowOptions {
            title: "Sign in".into(),
            width: 800.,
            height: 640.,
            min_size: Some((360., 280.)),
            bounds: popup_bounds,
            decorations: true,
            ..Default::default()
        },
        move |cx| {
            let closing_page = page.clone();
            cx.on_closed(move || {
                closing_page.lifecycle.set(BrowserLifecycle::Closing);
                if let Some(creation_id) = closing_page.creation_id.take() {
                    runtime::browser_creation_cancelled(creation_id);
                }
                if let Some(browser) = closing_page.browser.borrow().as_ref()
                    && let Some(host) = browser.host()
                {
                    host.close_browser(1);
                }
            });
            cx.render(content);
        },
    );
    *handle_slot.borrow_mut() = Some(window.clone());
    Some(PreparedPopup {
        client,
        window_info: WindowInfo {
            windowless_rendering_enabled: 1,
            shared_texture_enabled: 0,
            external_begin_frame_enabled: 0,
            ..Default::default()
        },
        window,
    })
}

pub(super) fn view(page: Rc<Page>) -> View {
    component(move |_| {
        let page = page.clone();
        let event_page = page.clone();
        let mount_page = page.clone();
        let visible_page = page.clone();
        let gpu = page.gpu.clone();
        div()
            .focusable(true)
            .overflow_hidden()
            .on_event(move |event| input(&event_page, event))
            .accessible_label("Web page")
            .on_mount(move |handle| {
                mount_page.node.set(Some(handle.node()));
                *mount_page.scene.borrow_mut() = handle
                    .scene()
                    .as_ref()
                    .map_or_else(Weak::new, Rc::downgrade);
                if mount_page.visible.replace(mount_page.visible.get() + 1) == 0
                    && let Some(browser) = mount_page.browser.borrow().as_ref()
                    && let Some(host) = browser.host()
                {
                    host.was_hidden(0);
                    host.invalidate(PaintElementType::VIEW);
                }
                handle.retain(Visible(visible_page.clone(), handle.node()));
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

pub(super) fn page_view(
    web: &WebView,
    dialogs: crate::FileDialogs,
    tasks: crate::TaskSpawner,
    windows: crate::WindowFactory,
) -> View {
    match backend(web, dialogs, tasks, windows) {
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

struct Visible(Rc<Page>, NodeId);
impl Drop for Visible {
    fn drop(&mut self) {
        if self.0.node.get() == Some(self.1) {
            self.0.node.set(None);
            *self.0.scene.borrow_mut() = Weak::new();
        }
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

impl Page {
    /// Account for a navigation: bump the document generation and fail every
    /// pending JavaScript evaluation as superseded. Called both from CEF's
    /// `on_load_start` and when a navigation command is applied, so the
    /// generation moves even if CEF's load-start callback is delayed (data:
    /// URLs) — an evaluation requested before the navigation must never
    /// resolve against the new document.
    fn note_navigation(&self) {
        self.navigation_generation
            .set(self.navigation_generation.get().wrapping_add(1));
        let replies = cancel_pending_evaluations(&self.evaluations);
        if !replies.is_empty() {
            zgx::native_view::post(move || {
                for reply in replies {
                    reply(Err(
                        "JavaScript evaluation was superseded by a navigation".into()
                    ));
                }
            });
        }
    }

    /// Convert a ZGX window point into the CEF view's logical coordinates.
    /// CEF's windowless input APIs are relative to the browser view, unlike
    /// ZGX input events which are relative to the native window.
    fn local(&self, x: f32, y: f32) -> Option<(f32, f32)> {
        let (node, scene) = (self.node.get()?, self.scene.borrow().upgrade()?);
        let scene = scene.try_borrow().ok()?;
        let bounds = scene.bounds(node);
        let shown = scene.window_rect(node, bounds);
        Some(window_to_local(
            x as f64,
            y as f64,
            shown.x as f64,
            shown.y as f64,
            shown.width as f64,
            shown.height as f64,
            bounds.width as f64,
            bounds.height as f64,
        ))
    }

    /// Convert a CEF view point back to ZGX window coordinates. CEF reports
    /// drag-source locations in view coordinates while ZGX starts its drag
    /// session with window coordinates.
    fn window_point(&self, x: f32, y: f32) -> Option<(f32, f32)> {
        let (node, scene) = (self.node.get()?, self.scene.borrow().upgrade()?);
        let scene = scene.try_borrow().ok()?;
        let bounds = scene.bounds(node);
        let shown = scene.window_rect(node, bounds);
        Some(local_to_window(
            x as f64,
            y as f64,
            shown.x as f64,
            shown.y as f64,
            shown.width as f64,
            shown.height as f64,
            bounds.width as f64,
            bounds.height as f64,
        ))
    }
}

fn input(page: &Page, event: &mut zgx::input::EventContext) {
    let input_event = &event.event;
    let physical_key = event.key_code().map(str::to_owned);
    let Some(browser) = page.browser.borrow().as_ref().cloned() else {
        return;
    };
    let Some(host) = browser.host() else {
        return;
    };
    match input_event.clone() {
        InputEvent::Drag(drag) => match drag.phase {
            DragPhase::Start => {
                if let Some(items) = page.drag_export.borrow_mut().take() {
                    event.set_drag_export(items);
                }
            }
            DragPhase::Over => {
                let Some(text) = drag.payload.downcast_ref::<DraggedText>() else {
                    return;
                };
                let Some(mouse) = window_mouse_event(page, drag.x, drag.y) else {
                    return;
                };
                if page.drag_target_data.borrow().is_none() {
                    let Some(mut data) = drag_data_for_text(text) else {
                        return;
                    };
                    host.drag_target_drag_enter(
                        Some(&mut data),
                        Some(&mouse),
                        DragOperationsMask::from(
                            cef::sys::cef_drag_operations_mask_t::DRAG_OPERATION_COPY,
                        ),
                    );
                    page.drag_target_data.replace(Some(data));
                    page.drag_target_files.borrow_mut().clear();
                } else {
                    host.drag_target_drag_over(
                        Some(&mouse),
                        DragOperationsMask::from(
                            cef::sys::cef_drag_operations_mask_t::DRAG_OPERATION_COPY,
                        ),
                    );
                }
                event.accept_drag();
            }
            DragPhase::Drop => {
                if page.drag_target_data.borrow().is_some()
                    && let Some(mouse) = window_mouse_event(page, drag.x, drag.y)
                {
                    host.drag_target_drop(Some(&mouse));
                    page.drag_target_data.borrow_mut().take();
                    event.prevent_default();
                    event.stop_propagation();
                }
            }
            DragPhase::Leave | DragPhase::End => {
                if page.drag_target_data.borrow_mut().take().is_some() {
                    host.drag_target_drag_leave();
                }
                page.drag_target_files.borrow_mut().clear();
                if drag.phase == DragPhase::End && page.drag_source_active.replace(false) {
                    let operation = if drag.accepted {
                        cef::sys::cef_drag_operations_mask_t::DRAG_OPERATION_COPY
                    } else {
                        cef::sys::cef_drag_operations_mask_t::DRAG_OPERATION_NONE
                    };
                    let (x, y) = page
                        .local(drag.x, drag.y)
                        .map(|(x, y)| (x as i32, y as i32))
                        .unwrap_or((drag.x as i32, drag.y as i32));
                    host.drag_source_ended_at(x, y, operation.into());
                    host.drag_source_system_drag_ended();
                }
            }
            _ => {}
        },
        InputEvent::FileHover { x, y, path } => {
            let Some(mouse) = window_mouse_event(page, x, y) else {
                return;
            };
            if page.drag_target_data.borrow().is_none() {
                let Some(mut data) = drag_data_create() else {
                    return;
                };
                if !add_file_to_drag_data(&data, &path) {
                    return;
                }
                host.drag_target_drag_enter(
                    Some(&mut data),
                    Some(&mouse),
                    DragOperationsMask::from(
                        cef::sys::cef_drag_operations_mask_t::DRAG_OPERATION_COPY,
                    ),
                );
                page.drag_target_data.replace(Some(data));
                page.drag_target_files.borrow_mut().insert(path);
            } else {
                if page.drag_target_files.borrow_mut().insert(path.clone())
                    && let Some(data) = page.drag_target_data.borrow().as_ref()
                {
                    let _ = add_file_to_drag_data(data, &path);
                }
                host.drag_target_drag_over(
                    Some(&mouse),
                    DragOperationsMask::from(
                        cef::sys::cef_drag_operations_mask_t::DRAG_OPERATION_COPY,
                    ),
                );
            }
            event.accept_drag();
        }
        InputEvent::FileHoverCancelled => {
            if page.drag_target_data.borrow_mut().take().is_some() {
                host.drag_target_drag_leave();
            }
            page.drag_target_files.borrow_mut().clear();
        }
        InputEvent::FileDrop { x, y, path } => {
            let Some(mouse) = window_mouse_event(page, x, y) else {
                return;
            };
            if page.drag_target_data.borrow().is_none() {
                let Some(mut data) = drag_data_create() else {
                    return;
                };
                if !add_file_to_drag_data(&data, &path) {
                    return;
                }
                host.drag_target_drag_enter(
                    Some(&mut data),
                    Some(&mouse),
                    DragOperationsMask::from(
                        cef::sys::cef_drag_operations_mask_t::DRAG_OPERATION_COPY,
                    ),
                );
                page.drag_target_data.replace(Some(data));
                page.drag_target_files.borrow_mut().insert(path);
            } else {
                let mut files = page.drag_target_files.borrow_mut();
                if !files.contains(&path)
                    && let Some(data) = page.drag_target_data.borrow().as_ref()
                    && add_file_to_drag_data(data, &path)
                {
                    files.insert(path);
                }
                host.drag_target_drag_over(
                    Some(&mouse),
                    DragOperationsMask::from(
                        cef::sys::cef_drag_operations_mask_t::DRAG_OPERATION_COPY,
                    ),
                );
            }
            if page.drag_target_data.borrow().is_some() {
                event.prevent_default();
            }
        }
        InputEvent::FilesDrop { x, y, paths } => {
            let Some(mouse) = window_mouse_event(page, x, y) else {
                return;
            };
            if page.drag_target_data.borrow().is_none() {
                let Some(mut data) = drag_data_create() else {
                    return;
                };
                let mut inserted = HashSet::new();
                for path in paths.iter() {
                    if !inserted.contains(path) && add_file_to_drag_data(&data, path) {
                        inserted.insert(path.clone());
                    }
                }
                if inserted.is_empty() {
                    return;
                }
                host.drag_target_drag_enter(
                    Some(&mut data),
                    Some(&mouse),
                    DragOperationsMask::from(
                        cef::sys::cef_drag_operations_mask_t::DRAG_OPERATION_COPY,
                    ),
                );
                page.drag_target_data.replace(Some(data));
                page.drag_target_files.replace(inserted);
            } else if let Some(data) = page.drag_target_data.borrow().as_ref() {
                let mut files = page.drag_target_files.borrow_mut();
                for path in paths.iter() {
                    if !files.contains(path) && add_file_to_drag_data(data, path) {
                        files.insert(path.clone());
                    }
                }
            }
            if page.drag_target_data.borrow().is_some() {
                host.drag_target_drop(Some(&mouse));
                page.drag_target_data.borrow_mut().take();
                page.drag_target_files.borrow_mut().clear();
                event.prevent_default();
                event.stop_propagation();
            }
        }
        InputEvent::FilesDropRejected { .. } => {
            if page.drag_target_data.borrow_mut().take().is_some() {
                host.drag_target_drag_leave();
            }
            page.drag_target_files.borrow_mut().clear();
        }
        InputEvent::PointerDown { x, y, button } | InputEvent::PointerUp { x, y, button } => {
            let mouse_up = matches!(input_event, InputEvent::PointerUp { .. });
            if button == PointerButton::Primary {
                page.pressed.set((!mouse_up).then_some((x, y)));
            }
            let (button_type, button_id, cef_flag) = match button {
                PointerButton::Primary => (
                    MouseButtonType::LEFT,
                    1,
                    cef::sys::cef_event_flags_t::EVENTFLAG_LEFT_MOUSE_BUTTON.0,
                ),
                PointerButton::Secondary => (
                    MouseButtonType::RIGHT,
                    2,
                    cef::sys::cef_event_flags_t::EVENTFLAG_RIGHT_MOUSE_BUTTON.0,
                ),
                PointerButton::Middle => (
                    MouseButtonType::MIDDLE,
                    3,
                    cef::sys::cef_event_flags_t::EVENTFLAG_MIDDLE_MOUSE_BUTTON.0,
                ),
                PointerButton::Other(_) => return,
            };
            let buttons = page.mouse_buttons.get();
            page.mouse_buttons
                .set(update_mouse_button_flags(buttons, cef_flag, !mouse_up));
            let click_count = if mouse_up {
                page.click_count.get()
            } else {
                let count = next_click_count(page, x as i32, y as i32, button_id);
                page.click_count.set(count);
                count
            };
            let Some(mouse) = window_mouse_event(page, x, y) else {
                return;
            };
            host.send_mouse_click_event(
                Some(&mouse),
                button_type,
                mouse_up as i32,
                click_count as i32,
            );
        }
        InputEvent::PointerMove { x, y } => {
            let Some(mouse) = window_mouse_event(page, x, y) else {
                return;
            };
            host.send_mouse_move_event(Some(&mouse), 0);
            if let Some(drag) = page.drag_source_pending.borrow_mut().take() {
                let origin = page
                    .pressed
                    .take()
                    .or_else(|| page.window_point(drag.x as f32, drag.y as f32))
                    .unwrap_or((0., 0.));
                page.drag_export.replace(Some(drag.items));
                page.drag_source_active.set(true);
                event.start_drag_from(
                    DraggedText::with_link_and_files(
                        drag.text,
                        drag.html,
                        drag.link_url,
                        drag.files,
                    ),
                    origin.0,
                    origin.1,
                );
            }
        }
        InputEvent::PointerLeave => host.send_mouse_move_event(Some(&MouseEvent::default()), 1),
        InputEvent::Scroll {
            x,
            y,
            delta_x,
            delta_y,
        } => host.send_mouse_wheel_event(
            window_mouse_event(page, x, y).as_ref(),
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
                    native_key_code: cef_native_key_code(&key, physical_key.as_deref())
                        .unwrap_or_default(),
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
                    native_key_code: cef_native_key_code(&key, physical_key.as_deref())
                        .unwrap_or_default(),
                    ..Default::default()
                }));
            }
        }
        _ => {}
    }
}

fn drag_data_for_text(text: &DraggedText) -> Option<DragData> {
    let data = drag_data_create()?;
    if !text.text().is_empty() {
        let fragment_text: CefString = text.text().into();
        data.set_fragment_text(Some(&fragment_text));
    }
    if let Some(html) = text.html() {
        let fragment_html: CefString = html.into();
        data.set_fragment_html(Some(&fragment_html));
    }
    if let Some(url) = text.link_url() {
        let url: CefString = url.into();
        data.set_link_url(Some(&url));
    }
    for path in text.files() {
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name() else {
            continue;
        };
        let path: CefString = path.to_string_lossy().as_ref().into();
        let name: CefString = name.to_string_lossy().as_ref().into();
        data.add_file(Some(&path), Some(&name));
    }
    Some(data)
}

fn add_file_to_drag_data(data: &DragData, path: &std::path::Path) -> bool {
    if !path.is_file() {
        return false;
    }
    let path_string: CefString = path.to_string_lossy().as_ref().into();
    let Some(file_name) = path.file_name() else {
        return false;
    };
    let display_name: CefString = file_name.to_string_lossy().as_ref().into();
    data.add_file(Some(&path_string), Some(&display_name));
    true
}

fn page_drag(data: &DragData, x: i32, y: i32) -> Option<PageDrag> {
    let url = CefString::from(&data.link_url()).to_string();
    let fragment_text = CefString::from(&data.fragment_text()).to_string();
    let fragment_html = CefString::from(&data.fragment_html()).to_string();
    let mut paths = CefStringList::new();
    let _ = data.file_paths(Some(&mut paths));
    let files = paths
        .into_iter()
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_file())
        .take(128)
        .collect();
    page_drag_fields_with_files(url, fragment_text, fragment_html, files, x, y)
}

#[cfg(test)]
fn page_drag_fields(
    url: String,
    fragment_text: String,
    fragment_html: String,
    x: i32,
    y: i32,
) -> Option<PageDrag> {
    page_drag_fields_with_files(url, fragment_text, fragment_html, Vec::new(), x, y)
}

fn page_drag_fields_with_files(
    url: String,
    fragment_text: String,
    fragment_html: String,
    files: Vec<std::path::PathBuf>,
    x: i32,
    y: i32,
) -> Option<PageDrag> {
    const MAX_FIELD_BYTES: usize = 1_048_576;
    let files: Vec<_> = files
        .into_iter()
        .filter(|path| path.is_file())
        .take(128)
        .collect();
    let text = if fragment_text.is_empty() {
        url.clone()
    } else {
        fragment_text
    };
    if (text.is_empty() && files.is_empty())
        || text.len() > MAX_FIELD_BYTES
        || url.len() > MAX_FIELD_BYTES
    {
        return None;
    }
    let html = (!fragment_html.is_empty() && fragment_html.len() <= MAX_FIELD_BYTES)
        .then_some(fragment_html);
    let mut items = Vec::with_capacity(2);
    if !url.is_empty() {
        items.push(zgx::input::DragItem::Url(url.clone()));
    }
    if !text.is_empty() {
        items.push(zgx::input::DragItem::Text(text.clone()));
    }
    items.extend(files.iter().cloned().map(zgx::input::DragItem::File));
    Some(PageDrag {
        text,
        html,
        link_url: (!url.is_empty()).then_some(url),
        files,
        items,
        x,
        y,
    })
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
        modifiers: cef_modifiers(page.input_modifiers.get()) | page.mouse_buttons.get(),
    }
}

fn window_mouse_event(page: &Page, x: f32, y: f32) -> Option<MouseEvent> {
    let (x, y) = page.local(x, y)?;
    Some(mouse_event(page, x, y))
}

#[allow(clippy::too_many_arguments)]
fn window_to_local(
    x: f64,
    y: f64,
    shown_x: f64,
    shown_y: f64,
    shown_width: f64,
    shown_height: f64,
    device_width: f64,
    device_height: f64,
) -> (f32, f32) {
    let sx = device_width / shown_width.max(0.001);
    let sy = device_height / shown_height.max(0.001);
    (((x - shown_x) * sx) as f32, ((y - shown_y) * sy) as f32)
}

#[allow(clippy::too_many_arguments)]
fn local_to_window(
    x: f64,
    y: f64,
    shown_x: f64,
    shown_y: f64,
    shown_width: f64,
    shown_height: f64,
    device_width: f64,
    device_height: f64,
) -> (f32, f32) {
    (
        (shown_x + x * shown_width / device_width.max(0.001)) as f32,
        (shown_y + y * shown_height / device_height.max(0.001)) as f32,
    )
}

fn update_mouse_button_flags(current: u32, button: u32, pressed: bool) -> u32 {
    if pressed {
        current | button
    } else {
        current & !button
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

fn cef_native_key_code(key: &Key, physical_key: Option<&str>) -> Option<i32> {
    #[cfg(target_os = "macos")]
    {
        let code = match key {
            Key::Character(value) => {
                physical_key.and_then(macos_physical_key_code).or_else(|| {
                    match value.to_ascii_lowercase().as_str() {
                        "a" => Some(0),
                        "s" => Some(1),
                        "d" => Some(2),
                        "f" => Some(3),
                        "h" => Some(4),
                        "g" => Some(5),
                        "z" => Some(6),
                        "x" => Some(7),
                        "c" => Some(8),
                        "v" => Some(9),
                        "b" => Some(11),
                        "q" => Some(12),
                        "w" => Some(13),
                        "e" => Some(14),
                        "r" => Some(15),
                        "y" => Some(16),
                        "t" => Some(17),
                        "1" => Some(18),
                        "2" => Some(19),
                        "3" => Some(20),
                        "4" => Some(21),
                        "6" => Some(22),
                        "5" => Some(23),
                        "=" | "+" => Some(24),
                        "9" => Some(25),
                        "7" => Some(26),
                        "-" | "_" => Some(27),
                        "8" => Some(28),
                        "0" => Some(29),
                        "]" | "}" => Some(30),
                        "o" => Some(31),
                        "u" => Some(32),
                        "[" | "{" => Some(33),
                        "i" => Some(34),
                        "p" => Some(35),
                        "l" => Some(37),
                        "j" => Some(38),
                        "'" | "\"" => Some(39),
                        "k" => Some(40),
                        ";" | ":" => Some(41),
                        "\\" | "|" => Some(42),
                        "," | "<" => Some(43),
                        "/" | "?" => Some(44),
                        "n" => Some(45),
                        "m" => Some(46),
                        "." | ">" => Some(47),
                        "`" | "~" => Some(50),
                        _ => None,
                    }
                })?
            }
            Key::Enter => 36,
            Key::Tab => 48,
            Key::Space => 49,
            Key::Backspace => 51,
            Key::Escape => 53,
            Key::ArrowLeft => 123,
            Key::ArrowRight => 124,
            Key::ArrowDown => 125,
            Key::ArrowUp => 126,
            Key::Home => 115,
            Key::End => 119,
            Key::PageUp => 116,
            Key::PageDown => 121,
            Key::Delete => 117,
            Key::Function(1) => 122,
            Key::Function(2) => 120,
            Key::Function(3) => 99,
            Key::Function(4) => 118,
            Key::Function(5) => 96,
            Key::Function(6) => 97,
            Key::Function(7) => 98,
            Key::Function(8) => 100,
            Key::Function(9) => 101,
            Key::Function(10) => 109,
            Key::Function(11) => 103,
            Key::Function(12) => 111,
            Key::Function(13) => 105,
            Key::Function(14) => 107,
            Key::Function(15) => 113,
            Key::Function(16) => 106,
            Key::Function(17) => 64,
            Key::Function(18) => 79,
            Key::Function(19) => 80,
            Key::Function(20) => 90,
            _ => return None,
        };
        Some(code)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (key, physical_key);
        None
    }
}

#[cfg(target_os = "macos")]
fn macos_physical_key_code(code: &str) -> Option<i32> {
    Some(match code {
        "KeyA" => 0,
        "KeyS" => 1,
        "KeyD" => 2,
        "KeyF" => 3,
        "KeyH" => 4,
        "KeyG" => 5,
        "KeyZ" => 6,
        "KeyX" => 7,
        "KeyC" => 8,
        "KeyV" => 9,
        "KeyB" => 11,
        "KeyQ" => 12,
        "KeyW" => 13,
        "KeyE" => 14,
        "KeyR" => 15,
        "KeyY" => 16,
        "KeyT" => 17,
        "Digit1" => 18,
        "Digit2" => 19,
        "Digit3" => 20,
        "Digit4" => 21,
        "Digit6" => 22,
        "Digit5" => 23,
        "Equal" => 24,
        "Digit9" => 25,
        "Digit7" => 26,
        "Minus" => 27,
        "Digit8" => 28,
        "Digit0" => 29,
        "BracketRight" => 30,
        "KeyO" => 31,
        "KeyU" => 32,
        "BracketLeft" => 33,
        "KeyI" => 34,
        "KeyP" => 35,
        "KeyL" => 37,
        "KeyJ" => 38,
        "Quote" => 39,
        "KeyK" => 40,
        "Semicolon" => 41,
        "Backslash" => 42,
        "Comma" => 43,
        "Slash" => 44,
        "KeyN" => 45,
        "KeyM" => 46,
        "Period" => 47,
        "Backquote" => 50,
        _ => return None,
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
        cef_key_code, cef_modifiers, cef_native_key_code, click_count, close_after_releasing_slot,
        local_to_window, page_drag_fields, page_drag_fields_with_files, text_key_events,
        utf16_offset, window_to_local,
    };
    use cef::KeyEventType;
    use zgx::input::{DraggedText, Key, Modifiers};

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
    fn macos_shortcut_keys_include_native_virtual_key_codes() {
        #[cfg(target_os = "macos")]
        {
            assert_eq!(
                cef_native_key_code(&Key::Character("c".into()), Some("KeyC")),
                Some(8)
            );
            assert_eq!(
                cef_native_key_code(&Key::Character("V".into()), Some("KeyV")),
                Some(9)
            );
            assert_eq!(
                cef_native_key_code(&Key::Character("é".into()), Some("KeyE")),
                Some(14)
            );
            assert_eq!(cef_native_key_code(&Key::Enter, Some("Enter")), Some(36));
            assert_eq!(
                cef_native_key_code(&Key::ArrowLeft, Some("ArrowLeft")),
                Some(123)
            );
            assert_eq!(cef_native_key_code(&Key::Character("é".into()), None), None);
        }
        #[cfg(not(target_os = "macos"))]
        assert_eq!(
            cef_native_key_code(&Key::Character("c".into()), Some("KeyC")),
            None
        );
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
    fn window_input_becomes_page_local_view_coordinates() {
        assert_eq!(
            window_to_local(140., 95., 40., 20., 200., 150., 400., 300.),
            (200., 150.)
        );
        assert_eq!(
            window_to_local(90., 45., 40., 20., 100., 50., 100., 50.),
            (50., 25.)
        );
        assert_eq!(
            local_to_window(200., 150., 40., 20., 200., 150., 400., 300.),
            (140., 95.)
        );
    }

    #[test]
    fn webpage_drag_keeps_text_html_and_link_for_other_webapps() {
        let drag = page_drag_fields(
            "https://example.test/document".into(),
            "Selected text".into(),
            "<b>Selected text</b>".into(),
            20,
            30,
        )
        .expect("valid web drag payload");

        assert_eq!(drag.text, "Selected text");
        assert_eq!(drag.html.as_deref(), Some("<b>Selected text</b>"));
        assert_eq!(
            drag.link_url.as_deref(),
            Some("https://example.test/document")
        );
        let payload =
            DraggedText::with_link(drag.text.clone(), drag.html.clone(), drag.link_url.clone());
        assert_eq!(payload.text(), "Selected text");
        assert_eq!(payload.html(), Some("<b>Selected text</b>"));
        assert_eq!(payload.link_url(), Some("https://example.test/document"));
        assert_eq!(drag.items.len(), 2);
        assert!(matches!(
            &drag.items[0],
            zgx::input::DragItem::Url(url) if url == "https://example.test/document"
        ));
        assert!(matches!(
            &drag.items[1],
            zgx::input::DragItem::Text(text) if text == "Selected text"
        ));
        assert_eq!((drag.x, drag.y), (20, 30));
    }

    #[test]
    fn webpage_drag_rejects_empty_and_oversized_payloads() {
        assert!(page_drag_fields(String::new(), String::new(), String::new(), 0, 0).is_none());
        assert!(
            page_drag_fields(String::new(), "x".repeat(1_048_577), String::new(), 0, 0,).is_none()
        );
    }

    #[test]
    fn webpage_file_drag_keeps_only_existing_paths_in_the_typed_payload() {
        let root = std::env::temp_dir().join(format!(
            "zgx-cef-page-drag-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let file = root.join("fixture.txt");
        std::fs::write(&file, b"fixture").unwrap();
        let missing = root.join("missing.txt");

        let drag = page_drag_fields_with_files(
            String::new(),
            String::new(),
            String::new(),
            vec![file.clone(), missing],
            12,
            34,
        )
        .expect("file-only drag is a valid payload");
        assert!(drag.text.is_empty());
        assert_eq!(drag.files, [file.clone()]);
        assert!(
            matches!(drag.items.as_slice(), [zgx::input::DragItem::File(path)] if path == &file)
        );

        let payload =
            DraggedText::with_link_and_files(drag.text, drag.html, drag.link_url, drag.files);
        assert_eq!(payload.files(), std::slice::from_ref(&file));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mouse_button_flags_follow_press_and_release_state() {
        let left = cef::sys::cef_event_flags_t::EVENTFLAG_LEFT_MOUSE_BUTTON.0;
        let right = cef::sys::cef_event_flags_t::EVENTFLAG_RIGHT_MOUSE_BUTTON.0;
        let left_pressed = super::update_mouse_button_flags(0, left, true);
        let both_pressed = super::update_mouse_button_flags(left_pressed, right, true);
        let right_released = super::update_mouse_button_flags(both_pressed, right, false);

        assert_eq!(left_pressed, left);
        assert_eq!(both_pressed, left | right);
        assert_eq!(right_released, left);
        assert_eq!(
            super::update_mouse_button_flags(right_released, left, false),
            0
        );
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

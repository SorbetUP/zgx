use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use cef::*;

const ENABLED_KEY: &str = "zgx_webview_messages";
const SCRIPTS_KEY: &str = "zgx_user_scripts";
const MESSAGE_NAME: &str = "zgx-webview-message";

/// Debug-only bridge diagnostics. `ZGX_CEF_BRIDGE_TRACE=/path` appends bounded
/// lifecycle lines (ids, booleans, error codes — never page content or URLs)
/// from both browser and renderer processes. Compiled out in release.
#[cfg(debug_assertions)]
pub(super) fn bridge_trace(message: &str) {
    let Some(path) = std::env::var_os("ZGX_CEF_BRIDGE_TRACE") else {
        return;
    };
    use std::io::Write;
    let line = format!("BRIDGE pid={} {message}\n", std::process::id());
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::path::PathBuf::from(path))
    {
        let _ = file.write_all(line.as_bytes());
    }
}
#[cfg(not(debug_assertions))]
pub(super) fn bridge_trace(_message: &str) {}

fn encode_script(source: &str, main_frame_only: bool) -> String {
    let scope = if main_frame_only { "main" } else { "all" };
    format!("{scope}\n{source}")
}

fn decode_script(entry: &str) -> Option<(String, bool)> {
    let (scope, source) = entry.split_once('\n')?;
    match scope {
        "main" => Some((source.to_owned(), false)),
        "all" => Some((source.to_owned(), true)),
        _ => None,
    }
}

#[derive(Clone, Default)]
struct RendererBrowserConfig {
    messages: bool,
    scripts: Vec<(String, bool)>,
}

/// Renderer-internal map of (browser id, frame id) to the security origin the
/// browser computed for that frame's current document. Captured from
/// `globalThis.origin` at context creation (see `on_context_created`); the
/// page cannot forge it, and it is never derived from the frame's URL. The
/// value stores `(origin, context raw, global-proxy raw)`: when a message's
/// receiver object is that recorded global proxy, `PostMessageHandler` reads
/// the LIVE `origin` off it (always the current document's origin) and
/// refreshes the entry. Entries are overwritten on each context creation and
/// never removed on release — removing there could drop a NEW document's
/// entry when an OLD context is released late; the map dies with the
/// renderer process, so stale entries are bounded.
type FrameOrigins = Arc<Mutex<HashMap<(i32, String), (String, usize, usize)>>>;

wrap_v8_handler! {
    struct PostMessageHandler { frame: Frame, browser_id: i32, frame_origins: FrameOrigins }
    impl V8Handler {
        fn execute(
            &self,
            name: Option<&CefString>,
            _object: Option<&mut V8Value>,
            arguments: Option<&[Option<V8Value>]>,
            _retval: Option<&mut Option<V8Value>>,
            _exception: Option<&mut CefString>,
        ) -> i32 {
            let name_ok = name.is_some_and(|name| name.to_string() == "__zgxNativePostMessage");
            bridge_trace(&format!(
                "v8-execute name_ok={name_ok} args={}",
                arguments.map_or(0, |arguments| arguments.len())
            ));
            if !name_ok {
                return 0;
            }
            let Some(Some(value)) = arguments.and_then(|arguments| arguments.first()) else {
                return 1;
            };
            if value.is_string() == 0 {
                return 1;
            }
            let body = CefString::from(&value.string_value()).to_string();
            if body.len() > 1_048_576 || serde_json::from_str::<serde_json::Value>(&body).is_err() {
                bridge_trace("v8-execute reject=body");
                return 1;
            }
            // SECURITY: attach the browser-computed security origin of this
            // frame's document as a second argument. The receiver object of a
            // normal `globalThis.__zgxNativePostMessage(...)` call IS the
            // frame's global proxy; when its raw pointer matches the one
            // recorded at context creation, we read `origin` live off the
            // receiver (unforgeable on the real global) — this is always the
            // CURRENT document's origin, even when the value captured at
            // context creation was still the initial document's. The stored
            // value is the fallback; a missing entry or a receiver that is
            // not the recorded global fails safe to "null" (opaque origin =
            // untrusted): e.g. a sandboxed iframe with an http(s) URL must
            // NOT be attributed that http origin, and a forged receiver must
            // not be able to claim any origin.
            let frame_id = CefString::from(&self.frame.identifier()).to_string();
            let object_raw = _object.as_ref().map(|object| object.get_raw() as usize);
            let mut origins = self
                .frame_origins
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let entry = origins.get(&(self.browser_id, frame_id.clone())).cloned();
            let origin = match (entry, object_raw) {
                (Some((stored, context_raw, global_raw)), Some(raw)) if raw == global_raw && global_raw != 0 => {
                    let origin_key: CefString = "origin".into();
                    let live = _object.and_then(|object| object.value_bykey(Some(&origin_key)))
                        .filter(|value| value.is_string() != 0)
                        .map(|value| CefString::from(&value.string_value()).to_string())
                        .filter(|origin| super::client::is_valid_security_origin(origin));
                    match live {
                        Some(live) => {
                            if live != stored {
                                origins.insert((self.browser_id, frame_id.clone()), (live.clone(), context_raw, global_raw));
                            }
                            live
                        }
                        None => stored,
                    }
                }
                (Some((stored, ..)), _) => stored,
                (None, _) => "null".to_owned(),
            };
            drop(origins);
            let name: CefString = MESSAGE_NAME.into();
            let Some(mut message) = process_message_create(Some(&name)) else {
                bridge_trace("v8-execute reject=create-message");
                return 1;
            };
            let Some(arguments) = message.argument_list() else {
                bridge_trace("v8-execute reject=arg-list");
                return 1;
            };
            if arguments.set_size(2) == 0 {
                bridge_trace("v8-execute reject=set-size");
                return 1;
            }
            let body: CefString = body.as_str().into();
            if arguments.set_string(0, Some(&body)) == 0 {
                bridge_trace("v8-execute reject=set-string");
                return 1;
            }
            let origin: CefString = origin.as_str().into();
            if arguments.set_string(1, Some(&origin)) == 0 {
                bridge_trace("v8-execute reject=set-origin");
                return 1;
            }
            self.frame
                .send_process_message(ProcessId::BROWSER, Some(&mut message));
            1
        }
    }
}

wrap_render_process_handler! {
    struct CEFRenderProcessHandler { browser_configs: Arc<Mutex<HashMap<i32, RendererBrowserConfig>>>, frame_origins: FrameOrigins }
    impl RenderProcessHandler {
        fn on_browser_created(
            &self,
            browser: Option<&mut Browser>,
            extra_info: Option<&mut DictionaryValue>,
        ) {
            let (Some(browser), Some(extra_info)) = (browser, extra_info) else {
                bridge_trace("on-browser-created missing-args");
                return;
            };
            let key: CefString = ENABLED_KEY.into();
            let mut config = RendererBrowserConfig {
                messages: extra_info.bool(Some(&key)) != 0,
                scripts: Vec::new(),
            };
            bridge_trace(&format!(
                "on-browser-created id={} messages={}",
                browser.identifier(),
                config.messages
            ));
            let scripts_key: CefString = SCRIPTS_KEY.into();
            if let Some(scripts) = extra_info.list(Some(&scripts_key)) {
                for index in 0..scripts.size() {
                    let entry = CefString::from(&scripts.string(index)).to_string();
                    if let Some(script) = decode_script(&entry) {
                        config.scripts.push(script);
                    }
                }
            }
            self.browser_configs
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .insert(browser.identifier(), config);
            // frame_origins entries are never removed here: CEF can invoke
            // browser/context callbacks while frames are already torn down,
            // and removing entries for a browser whose renderer process
            // still serves other frames loses live origins. Liveness is
            // enforced at message time instead (see PostMessageHandler).
        }

        fn on_browser_destroyed(&self, browser: Option<&mut Browser>) {
            let Some(browser) = browser else { return };
            let browser_id = browser.identifier();
            self.browser_configs
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .remove(&browser_id);
            // Do NOT retain() frame_origins here: CEF can invoke this while
            // frames are already torn down, and removing entries for a
            // browser whose renderer still runs other frames is wrong.
            // Stale entries are filtered lazily in on_browser_created.
        }

        fn on_context_created(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            context: Option<&mut V8Context>,
        ) {
            let (Some(browser), Some(frame), Some(context)) = (browser, frame, context) else {
                bridge_trace("on-context-created missing-args");
                return;
            };
            let browser_id = browser.identifier();
            let frame_id = CefString::from(&frame.identifier()).to_string();
            let main_frame = frame.is_main() != 0;
            // SECURITY: capture the security origin of this frame's document
            // as computed by the browser itself. `globalThis.origin` is the
            // serialized origin of the document's origin — unforgeable from
            // page JS — and is "null" for opaque origins (sandboxed iframes,
            // data:/about: documents). This is deliberately NOT derived from
            // frame.url(): about:srcdoc/about:blank inherit the parent origin,
            // a sandboxed iframe has an opaque origin even with an http(s)
            // URL, and blob: carries its creator's origin. The value is stored
            // renderer-side and attached to page messages by
            // PostMessageHandler, so the page cannot claim another origin.
            if context.enter() != 0 {
                let code: CefString = "String(globalThis.origin)".into();
                let code_url: CefString = "zgx://bridge/origin".into();
                let mut result = None;
                let mut exception = None;
                context.eval(
                    Some(&code),
                    Some(&code_url),
                    1,
                    Some(&mut result),
                    Some(&mut exception),
                );
                context.exit();
                let origin = result
                    .as_ref()
                    .filter(|value| value.is_string() != 0)
                    .map(|value| CefString::from(&value.string_value()).to_string())
                    .filter(|origin| super::client::is_valid_security_origin(origin))
                    .unwrap_or_else(|| "null".to_owned());
                // Capture the raw pointer of the global PROXY (`globalThis`),
                // which is what a normal `globalThis.__zgxNativePostMessage()`
                // call passes as the receiver — unlike context.global(), which
                // may be the unwrapped global object with a different pointer.
                let proxy_code: CefString = "globalThis".into();
                let mut proxy_result = None;
                let mut proxy_exception = None;
                context.eval(
                    Some(&proxy_code),
                    Some(&code_url),
                    1,
                    Some(&mut proxy_result),
                    Some(&mut proxy_exception),
                );
                let global_raw = proxy_result
                    .as_ref()
                    .map(|value| value.get_raw() as usize)
                    .unwrap_or(0);
                self.frame_origins
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .insert((browser_id, frame_id.clone()), (origin.clone(), context.get_raw() as usize, global_raw));
                bridge_trace(&format!(
                    "on-context-created origin id={browser_id} frame={frame_id} main={main_frame} opaque={}",
                    origin == "null"
                ));
            } else {
                bridge_trace(&format!("on-context-created origin-enter-failed id={browser_id} frame={frame_id}"));
            }
            let Some(config) = self
                .browser_configs
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .get(&browser_id)
                .cloned()
            else {
                bridge_trace(&format!("on-context-created id={browser_id} main={main_frame} no-config"));
                return;
            };
            bridge_trace(&format!(
                "on-context-created id={browser_id} main={main_frame} messages={}",
                config.messages
            ));

            if config.messages {
                let Some(global) = context.global() else {
                    bridge_trace("on-context-created no-global");
                    return;
                };
                let mut handler =
                    PostMessageHandler::new(frame.clone(), browser_id, self.frame_origins.clone());
                let native_name: CefString = "__zgxNativePostMessage".into();
                let mut native = match v8_value_create_function(Some(&native_name), Some(&mut handler)) {
                    Some(native) => native,
                    None => {
                        bridge_trace("on-context-created create-function-failed");
                        return;
                    }
                };
                let installed = global.set_value_bykey(
                    Some(&native_name),
                    Some(&mut native),
                    V8Propertyattribute::default(),
                );
                bridge_trace(&format!("on-context-created installed-native={installed}"));

                let source = r#"(() => {
              const send = value => globalThis.__zgxNativePostMessage(JSON.stringify(value));
              let webkit = globalThis.webkit;
              if (!webkit || (typeof webkit !== 'object' && typeof webkit !== 'function')) webkit = {};
              let handlers = webkit.messageHandlers;
              if (!handlers || (typeof handlers !== 'object' && typeof handlers !== 'function')) handlers = {};
              Object.defineProperty(handlers, 'zgx', { configurable: true, value: Object.freeze({ postMessage: send }) });
              Object.defineProperty(webkit, 'messageHandlers', { configurable: true, value: handlers });
              if (globalThis.webkit !== webkit) Object.defineProperty(globalThis, 'webkit', { configurable: true, value: webkit });
              let zgx = globalThis.zgx;
              if (!zgx || (typeof zgx !== 'object' && typeof zgx !== 'function')) zgx = {};
              if (typeof zgx.postMessage !== 'function') Object.defineProperty(zgx, 'postMessage', { configurable: true, value: send });
              if (globalThis.zgx !== zgx) Object.defineProperty(globalThis, 'zgx', { configurable: true, value: zgx });
            })();"#;
                let source: CefString = source.into();
                let source_url: CefString = "zgx://bridge/bootstrap".into();
                let mut result = None;
                let mut exception = None;
                let entered = context.enter();
                if entered != 0 {
                    context.eval(
                        Some(&source),
                        Some(&source_url),
                        1,
                        Some(&mut result),
                        Some(&mut exception),
                    );
                    context.exit();
                    let exception_text = exception
                        .as_ref()
                        .map(|exception| CefString::from(&exception.message()).to_string())
                        .unwrap_or_else(|| "none".into());
                    bridge_trace(&format!(
                        "bootstrap exception={exception_text} result={}",
                        result.is_some()
                    ));
                } else {
                    bridge_trace(&format!("bootstrap enter-failed code={entered}"));
                }
            }

            for (index, (script, all_frames)) in config.scripts.iter().enumerate() {
                if !all_frames && frame.is_main() == 0 {
                    continue;
                }
                let script: CefString = script.as_str().into();
                let source_url = format!("zgx://user-script/{index}");
                let source_url: CefString = source_url.as_str().into();
                let mut result = None;
                let mut exception = None;
                if context.enter() != 0 {
                    let _ = context.eval(Some(&script), Some(&source_url), 1, Some(&mut result), Some(&mut exception));
                    context.exit();
                }
            }
        }

        fn on_context_released(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _context: Option<&mut V8Context>,
        ) {
            // A frame's origin entry is deliberately kept here: it is
            // overwritten by the next on_context_created for that frame, and
            // removing on release risks dropping the NEW document's entry
            // when the OLD context is released late. The map lives and dies
            // with the renderer process, so stale entries are bounded.
        }
    }
}

pub(super) fn render_process_handler() -> RenderProcessHandler {
    CEFRenderProcessHandler::new(
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
    )
}

pub(super) fn extra_info(options: &zgx::web_view::WebViewOptions) -> Option<DictionaryValue> {
    let extra = dictionary_value_create()?;
    let key: CefString = ENABLED_KEY.into();
    extra.set_bool(Some(&key), options.messages as i32);
    let scripts_key: CefString = SCRIPTS_KEY.into();
    let mut scripts = list_value_create()?;
    if scripts.set_size(options.scripts.len()) == 0 {
        return None;
    }
    for (index, script) in options.scripts.iter().enumerate() {
        let encoded = encode_script(&script.source, script.main_frame_only);
        let encoded: CefString = encoded.as_str().into();
        if scripts.set_string(index, Some(&encoded)) == 0 {
            return None;
        }
    }
    if extra.set_list(Some(&scripts_key), Some(&mut scripts)) == 0 {
        return None;
    }
    Some(extra)
}

pub(super) fn message_name() -> &'static str {
    MESSAGE_NAME
}

#[cfg(test)]
mod tests {
    use super::{decode_script, encode_script};

    #[test]
    fn user_scripts_preserve_source_and_frame_scope() {
        for (source, main_frame_only) in [
            ("document.title = 'ready'", true),
            ("window.fixture = 1;\nwindow.other = 2", false),
        ] {
            let encoded = encode_script(source, main_frame_only);
            assert_eq!(
                decode_script(&encoded),
                Some((source.into(), !main_frame_only))
            );
        }
        assert_eq!(decode_script("unknown\nscript"), None);
        assert_eq!(decode_script("missing-scope"), None);
    }
}

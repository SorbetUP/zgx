use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use cef::*;

const ENABLED_KEY: &str = "zgx_webview_messages";
const SCRIPTS_KEY: &str = "zgx_user_scripts";
const MESSAGE_NAME: &str = "zgx-webview-message";

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

wrap_v8_handler! {
    struct PostMessageHandler { frame: Frame }
    impl V8Handler {
        fn execute(
            &self,
            name: Option<&CefString>,
            _object: Option<&mut V8Value>,
            arguments: Option<&[Option<V8Value>]>,
            _retval: Option<&mut Option<V8Value>>,
            _exception: Option<&mut CefString>,
        ) -> i32 {
            if name.is_none_or(|name| name.to_string() != "__zgxNativePostMessage") {
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
                return 1;
            }
            let name: CefString = MESSAGE_NAME.into();
            let Some(mut message) = process_message_create(Some(&name)) else { return 1 };
            let Some(arguments) = message.argument_list() else { return 1 };
            if arguments.set_size(1) == 0 { return 1; }
            let body: CefString = body.as_str().into();
            if arguments.set_string(0, Some(&body)) == 0 { return 1; }
            self.frame.send_process_message(ProcessId::BROWSER, Some(&mut message));
            1
        }
    }
}

wrap_render_process_handler! {
    struct CEFRenderProcessHandler { browser_configs: Arc<Mutex<HashMap<i32, RendererBrowserConfig>>> }
    impl RenderProcessHandler {
        fn on_browser_created(
            &self,
            browser: Option<&mut Browser>,
            extra_info: Option<&mut DictionaryValue>,
        ) {
            let (Some(browser), Some(extra_info)) = (browser, extra_info) else { return };
            let key: CefString = ENABLED_KEY.into();
            let mut config = RendererBrowserConfig {
                messages: extra_info.bool(Some(&key)) != 0,
                scripts: Vec::new(),
            };
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
        }

        fn on_browser_destroyed(&self, browser: Option<&mut Browser>) {
            let Some(browser) = browser else { return };
            self.browser_configs
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .remove(&browser.identifier());
        }

        fn on_context_created(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            context: Option<&mut V8Context>,
        ) {
            let (Some(browser), Some(frame), Some(context)) = (browser, frame, context) else { return };
            let Some(config) = self
                .browser_configs
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .get(&browser.identifier())
                .cloned()
            else {
                return;
            };

            if config.messages {
                let Some(global) = context.global() else { return };
                let mut handler = PostMessageHandler::new(frame.clone());
                let native_name: CefString = "__zgxNativePostMessage".into();
                let mut native = match v8_value_create_function(Some(&native_name), Some(&mut handler)) {
                    Some(native) => native,
                    None => return,
                };
                let _installed = global.set_value_bykey(
                    Some(&native_name),
                    Some(&mut native),
                    V8Propertyattribute::default(),
                );

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
                if context.enter() != 0 {
                    let _evaluated = context.eval(
                        Some(&source),
                        Some(&source_url),
                        1,
                        Some(&mut result),
                        Some(&mut exception),
                    );
                    context.exit();
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
    }
}

pub(super) fn render_process_handler() -> RenderProcessHandler {
    CEFRenderProcessHandler::new(Arc::new(Mutex::new(HashMap::new())))
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

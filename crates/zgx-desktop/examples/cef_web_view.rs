// Local CEF OSR smoke page. Run from a CEF bundle so Chromium's framework,
// resources and sandbox helper are beside the app executable:
// `cargo run -p zgx-desktop --features cef --bin cef_web_view`.
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    sync::{Arc, Mutex},
    thread,
};
use zgx::{
    compose::prelude::*,
    input::{InputEvent, PointerButton},
    web_view::{FindOptions, WebProfile, WebView, WebViewOptions},
};
use zgx_desktop::{Application, WindowBounds, WindowOptions};

const DOWNLOAD_PAYLOAD: &[u8] = b"cef-download-payload";

const FIXTURE: &str = r#"<!doctype html>
<meta charset="utf-8"><title>CEF OSR fixture</title>
<style>
  html,body{margin:0;width:100%;height:100%;font:18px system-ui;color:white}
  body{background:#17324d;display:grid;place-items:center}
  main{width:80%;height:80%;display:grid;grid-template-columns:1fr 1fr;grid-template-rows:1fr 1fr;gap:8px}
  div{display:grid;place-items:center;font-weight:700}
  #red{background:#d92745}#green{background:#1a9b68}#blue{background:#246bd1}#gold{background:#e5ad28;color:#142033}
  input{font:inherit;padding:8px}
</style>
<main><div id="red">RED PIXEL</div><div id="green">GREEN PIXEL</div>
<div id="blue"><input id="typing" aria-label="typing fixture" placeholder="click and type"></div>
<div id="gold"><button onclick="location.hash='navigated'">Navigate</button>
<a id="download" href="/download">Download fixture</a></div></main>
"#;

const EVENT_FIXTURE: &str = r#"<!doctype html>
<meta charset="utf-8"><title>CEF event initial</title>
<p>CEF navigation event fixture</p>
"#;

const EVENT_CHANGED_FIXTURE: &str = r#"<!doctype html>
<meta charset="utf-8"><title>CEF event changed</title>
<p>cef-event-url-changed</p>
<iframe srcdoc="<script>window.webkit.messageHandlers.zgx.postMessage({source:'iframe',value:7})</script>"></iframe>
<script>window.webkit.messageHandlers.zgx.postMessage({source:"cef-fixture",value:42})</script>
"#;

fn run_evaluation_test(
    web: WebView,
    result_path: std::path::PathBuf,
    window: zgx_desktop::WindowHandle,
    done: Arc<AtomicBool>,
) {
    let remaining = Arc::new(AtomicUsize::new(9));
    let all_ok = Arc::new(AtomicBool::new(true));
    let cases = [
        ("1 + 2", Ok("3")),
        ("document.title", Ok("\"CEF event changed\"")),
        ("'result'", Ok("\"result\"")),
        (
            "({name:'polyscale',value:42})",
            Ok("{\"name\":\"polyscale\",\"value\":42}"),
        ),
        ("[1,2,3]", Ok("[1,2,3]")),
        ("undefined", Ok("null")),
        ("null", Ok("null")),
        ("throw new Error('fixture')", Err("fixture")),
    ];
    for (script, expected) in cases {
        let remaining = remaining.clone();
        let all_ok = all_ok.clone();
        let result_path = result_path.clone();
        let log_path = result_path.with_extension("log");
        let window = window.clone();
        let done = done.clone();
        web.evaluate_js(script, move |result| {
            if let Ok(mut log) = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_path)
            {
                let _ = writeln!(log, "{script}: {result:?} expected={expected:?}");
            }
            let matches = match (result, expected) {
                (Ok(actual), Ok(expected)) => actual == expected,
                (Err(actual), Err(expected)) => actual.contains(expected),
                _ => false,
            };
            if !matches {
                all_ok.store(false, Ordering::Release);
            }
            if remaining.fetch_sub(1, Ordering::AcqRel) == 1 && !done.swap(true, Ordering::AcqRel) {
                let result = if all_ok.load(Ordering::Acquire) {
                    "PASS"
                } else {
                    "FAIL"
                };
                let _ = fs::write(result_path, result);
                window.close();
            }
        });
    }
    let remaining_find = remaining.clone();
    let all_ok_find = all_ok.clone();
    let result_path_find = result_path.clone();
    let done_find = done.clone();
    web.find(
        "cef-event-url-changed",
        FindOptions::default(),
        move |result| {
            let matched = result.found;
            if let Ok(mut log) = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(result_path_find.with_extension("log"))
            {
                let _ = writeln!(log, "find cef-event-url-changed: {result:?}");
            }
            if !matched {
                all_ok_find.store(false, Ordering::Release);
            }
            if remaining_find.fetch_sub(1, Ordering::AcqRel) == 1
                && !done_find.swap(true, Ordering::AcqRel)
            {
                let result = if all_ok_find.load(Ordering::Acquire) {
                    "PASS"
                } else {
                    "FAIL"
                };
                let _ = fs::write(result_path, result);
                window.close();
            }
        },
    );
}

fn start_fixture_server(
    download_test: bool,
    site_data_test: bool,
) -> Result<String, Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let address = listener.local_addr()?;
    let origin = format!("http://{address}");
    let result_path = std::env::var_os("ZGX_CEF_DOWNLOAD_RESULT").map(std::path::PathBuf::from);
    let site_data_result_path =
        std::env::var_os("ZGX_CEF_SITE_DATA_RESULT").map(std::path::PathBuf::from);
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let result_path = result_path.clone();
            let site_data_result_path = site_data_result_path.clone();
            thread::spawn(move || {
                let mut stream = stream;
                let mut request = [0; 2048];
                let count = stream.read(&mut request).unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..count]);
                let path = request.split_whitespace().nth(1).unwrap_or("/");
                if let Some(result_path) = site_data_result_path.as_ref() {
                    if let Ok(mut result) = fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(result_path)
                    {
                        let _ = writeln!(result, "HTTP:{path}");
                    }
                }
                if let Some(result_path) = result_path.as_ref() {
                    if let Ok(mut result) = fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(result_path)
                    {
                        let _ = writeln!(result, "HTTP:{path} download={download_test}");
                    }
                }
                let (status, content_type, headers, body) = if site_data_test
                    && path == "/site-data"
                {
                    (
                        "200 OK",
                        "text/html; charset=utf-8",
                        "Set-Cookie: fixture_cookie=connected; HttpOnly; SameSite=Lax\r\n",
                        b"<!doctype html><meta charset=utf-8><title>site-data</title><script>localStorage.setItem('polyscale-site-data','present')</script>".as_slice(),
                    )
                } else if site_data_test && path == "/cookie-check" {
                    let has_cookie = request.lines().any(|line| {
                        line.to_ascii_lowercase().starts_with("cookie:")
                            && line.contains("fixture_cookie=connected")
                    });
                    let body: &'static [u8] = if has_cookie { b"present" } else { b"absent" };
                    ("200 OK", "text/plain", "", body)
                } else if path == "/event-changed" {
                    (
                        "200 OK",
                        "text/html; charset=utf-8",
                        "",
                        EVENT_CHANGED_FIXTURE.as_bytes(),
                    )
                } else if path == "/event" {
                    (
                        "200 OK",
                        "text/html; charset=utf-8",
                        "",
                        EVENT_FIXTURE.as_bytes(),
                    )
                } else if download_test && path == "/download" {
                    (
                        "200 OK",
                        "text/plain",
                        "Content-Disposition: attachment; filename=\"cef-fixture.txt\"\r\n",
                        DOWNLOAD_PAYLOAD,
                    )
                } else {
                    ("200 OK", "text/html; charset=utf-8", "", FIXTURE.as_bytes())
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(body);
            });
        }
    });
    Ok(origin)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // This executable is a development fixture, not a distributed app. Keep
    // its Chromium test profile away from the user's Keychain so repeated
    // launches do not prompt for "Chromium Safe Storage" access.
    let fixture_data_dir =
        std::env::temp_dir().join(format!("zgx-cef-fixture-{}", std::process::id()));
    unsafe {
        std::env::set_var("ZGX_CEF_TEST_FIXTURE", "1");
        std::env::set_var("POLYSCALE_CEF_DATA_DIR", &fixture_data_dir);
    }
    let download_result_path =
        std::env::var_os("ZGX_CEF_DOWNLOAD_RESULT").map(std::path::PathBuf::from);
    let download_test = download_result_path.is_some();
    let event_result_path = std::env::var_os("ZGX_CEF_EVENT_RESULT").map(std::path::PathBuf::from);
    let event_test = event_result_path.is_some();
    let eval_result_path = std::env::var_os("ZGX_CEF_EVAL_RESULT").map(std::path::PathBuf::from);
    let eval_test = eval_result_path.is_some();
    let site_data_result_path =
        std::env::var_os("ZGX_CEF_SITE_DATA_RESULT").map(std::path::PathBuf::from);
    let site_data_test = site_data_result_path.is_some();
    let late_load_result_path =
        std::env::var_os("ZGX_CEF_LATE_LOAD_RESULT").map(std::path::PathBuf::from);
    let late_load_test = late_load_result_path.is_some();
    let automated = download_test || event_test || eval_test || site_data_test || late_load_test;
    if let Some(result_path) = download_result_path.as_ref() {
        fs::write(result_path, format!("START:auto={automated}\n"))?;
    }
    if let Some(result_path) = event_result_path.as_ref() {
        fs::write(result_path, "START\n")?;
    }
    if let Some(result_path) = eval_result_path.as_ref() {
        fs::write(result_path, "START\n")?;
    }
    if let Some(result_path) = site_data_result_path.as_ref() {
        fs::write(result_path, "START\n")?;
    }
    let origin = start_fixture_server(download_test, site_data_test || late_load_test)?;
    if let Some(result_path) = event_result_path.as_ref() {
        let mut result = fs::OpenOptions::new().append(true).open(result_path)?;
        writeln!(result, "ORIGIN:{origin}")?;
    }
    let run_result = Application::new()
        .window(WindowOptions {
            title: "CEF OSR fixture".into(),
            width: 900.,
            height: 650.,
            // Keep automated fixtures out of view while retaining an active
            // native window. Some platforms pause timers on hidden windows.
            visible: true,
            bounds: automated.then(|| WindowBounds::new(900, 650).at(-10_000, -10_000)),
            ..Default::default()
        })
        .run(|cx| {
            let profile = if late_load_test {
                WebProfile::Named("zgx-late-load-fixture".into())
            } else {
                WebProfile::Default
            };
            let web = WebView::new(
                &cx.ui.runtime,
                WebViewOptions {
                    profile,
                    ..Default::default()
                },
            );
            let late_shared = late_load_test.then(|| {
                WebView::new(
                    &cx.ui.runtime,
                    WebViewOptions {
                        profile: WebProfile::Named("zgx-late-load-fixture".into()),
                        ..Default::default()
                    },
                )
            });
            let late_isolated = late_load_test.then(|| {
                WebView::new(
                    &cx.ui.runtime,
                    WebViewOptions {
                        profile: WebProfile::Named("zgx-late-isolated-fixture".into()),
                        ..Default::default()
                    },
                )
            });
            let download_path =
                std::env::temp_dir().join(format!("zgx-cef-download-{}.txt", std::process::id()));
            let _ = fs::remove_file(&download_path);
            let completed_path = download_path.clone();
            let window = cx.window.clone();
            let result_path = download_result_path.clone();
            web.set_download_handler(move |request, responder| {
                println!("CEF_DOWNLOAD_REQUEST {} {}", request.name, request.mime);
                let completed_path = completed_path.clone();
                let result_path = result_path.clone();
                let window = window.clone();
                if let Some(result_path) = result_path.as_ref() {
                    let _ = fs::write(
                        result_path,
                        format!("REQUEST:{}:{}", request.name, request.mime),
                    );
                }
                responder.save_to(download_path.clone(), move |status| match status {
                    zgx::web_view::DownloadStatus::Progress { received, total } => {
                        println!("CEF_DOWNLOAD_PROGRESS {received} {total:?}");
                    }
                    zgx::web_view::DownloadStatus::Finished => {
                        let matches =
                            fs::read(&completed_path).is_ok_and(|bytes| bytes == DOWNLOAD_PAYLOAD);
                        println!("CEF_DOWNLOAD_{}", if matches { "PASS" } else { "FAIL" });
                        if let Some(result_path) = result_path.as_ref() {
                            let _ = fs::write(result_path, if matches { "PASS" } else { "FAIL" });
                            window.close();
                        }
                        let _ = fs::remove_file(&completed_path);
                    }
                    zgx::web_view::DownloadStatus::Failed(reason) => {
                        eprintln!("CEF_DOWNLOAD_FAILED {reason}");
                    }
                    _ => eprintln!("CEF_DOWNLOAD_UNKNOWN_STATUS"),
                });
            });
            let event_subscription = event_result_path.map(|result_path| {
                let observed = Arc::new(Mutex::new((false, false, false)));
                let event_state = observed.clone();
                let event_done = Arc::new(AtomicBool::new(false));
                let completed = event_done.clone();
                let log_path = result_path.clone();
                let callback_result_path = result_path.clone();
                let window = cx.window.clone();
                let expected_origin = "null".to_owned();
                let subscription = web.on_event(move |event| {
                    let label = match event {
                        zgx::web_view::WebViewEvent::TitleChanged { title }
                            if title == "CEF event changed" =>
                        {
                            "TITLE"
                        }
                        zgx::web_view::WebViewEvent::Message {
                            body,
                            origin,
                            main_frame,
                        } if origin == &expected_origin
                            && *main_frame
                            && body == r#"{"source":"cef-fixture","value":42}"# =>
                        {
                            "MESSAGE"
                        }
                        zgx::web_view::WebViewEvent::Message {
                            body,
                            origin,
                            main_frame,
                        } if origin == &expected_origin
                            && !*main_frame
                            && body == r#"{"source":"iframe","value":7}"# =>
                        {
                            "IFRAME"
                        }
                        _ => "OTHER",
                    };
                    if let Ok(mut log) = fs::OpenOptions::new().append(true).open(&log_path) {
                        let _ = writeln!(log, "EVENT:{label}:{event:?}");
                    }
                    let mut observed = event_state
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner());
                    match event {
                        zgx::web_view::WebViewEvent::TitleChanged { title }
                            if title == "CEF event changed" =>
                        {
                            observed.0 = true
                        }
                        zgx::web_view::WebViewEvent::Message {
                            body,
                            origin,
                            main_frame,
                        } if origin == &expected_origin
                            && *main_frame
                            && body == r#"{"source":"cef-fixture","value":42}"# =>
                        {
                            observed.1 = true
                        }
                        zgx::web_view::WebViewEvent::Message {
                            body,
                            origin,
                            main_frame,
                        } if origin == &expected_origin
                            && !*main_frame
                            && body == r#"{"source":"iframe","value":7}"# =>
                        {
                            observed.2 = true
                        }
                        _ => {}
                    }
                    if observed.0 && observed.1 && observed.2 {
                        event_done.store(true, Ordering::Release);
                        let _ = fs::write(&callback_result_path, "PASS");
                        window.close();
                    }
                });
                let timeout_window = cx.window.clone();
                let timeout_path = result_path;
                cx.tasks.spawn(async move {
                    zgx::timer::sleep(std::time::Duration::from_secs(12)).await;
                    if !completed.load(Ordering::Acquire) {
                        let _ = fs::write(timeout_path, "TIMEOUT");
                        timeout_window.close();
                    }
                });
                subscription
            });
            let eval_done = Arc::new(AtomicBool::new(false));
            if late_load_test {
                let pages = [
                    web.clone(),
                    late_shared.clone().expect("late shared page exists"),
                    late_isolated.clone().expect("late isolated page exists"),
                ];
                let result_path = late_load_result_path
                    .clone()
                    .expect("late-load test has a result path");
                let window = cx.window.clone();
                cx.tasks.spawn(async move {
                    // Exercise three browsers created together, including two
                    // pages sharing a named context and one isolated context.
                    zgx::timer::sleep(std::time::Duration::from_millis(300)).await;
                    for page in &pages {
                        page.load_url(format!("{origin}/site-data"));
                    }
                    for _ in 0..120 {
                        if pages.iter().all(|page| page.title() == "site-data") {
                            break;
                        }
                        zgx::timer::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    if let Ok(mut result) = fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&result_path)
                    {
                        for page in &pages {
                            let _ = writeln!(
                                result,
                                "ATTACHED:{}",
                                page.is_attached()
                            );
                        }
                        for page in &pages {
                            let _ = writeln!(result, "TITLE:{:?}", page.title());
                        }
                    }
                    let remaining = Arc::new(AtomicUsize::new(pages.len()));
                    let all_ok = Arc::new(AtomicBool::new(true));
                    for page in pages {
                        let remaining = remaining.clone();
                        let all_ok = all_ok.clone();
                        let result_path = result_path.clone();
                        let window = window.clone();
                        // Keep the controller alive until its asynchronous
                        // DevTools reply arrives; this also makes early-drop
                        // lifecycle failures visible rather than racy.
                        let keep_alive = page.clone();
                        page.evaluate_js("document.title", move |title| {
                            let _keep_alive = keep_alive;
                            let matches = title.as_deref() == Ok("\"site-data\"");
                            if let Ok(mut result) = fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(&result_path)
                            {
                                let _ = writeln!(result, "EVAL:{title:?}:matches={matches}");
                            }
                            if !matches {
                                all_ok.store(false, Ordering::Release);
                            }
                            if remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
                                if let Ok(mut result) = fs::OpenOptions::new()
                                    .create(true)
                                    .append(true)
                                    .open(result_path)
                                {
                                    let _ = writeln!(
                                        result,
                                        "STATUS:{}",
                                        if all_ok.load(Ordering::Acquire) {
                                            "PASS"
                                        } else {
                                            "FAIL"
                                        }
                                    );
                                }
                                window.close();
                            }
                        });
                    }
                });
            } else if site_data_test {
                let site_web = web.clone();
                let site_origin = origin.clone();
                let result_path = site_data_result_path
                    .clone()
                    .expect("site-data test has a result path");
                let window = cx.window.clone();
                let site_test_done = Arc::new(AtomicBool::new(false));
                let timeout_done = site_test_done.clone();
                let timeout_path = result_path.clone();
                let timeout_window = window.clone();
                let callback_done = site_test_done.clone();
                cx.tasks.spawn(async move {
                    zgx::timer::sleep(std::time::Duration::from_secs(15)).await;
                    if !timeout_done.swap(true, Ordering::AcqRel) {
                        let _ = fs::write(timeout_path, "TIMEOUT:site-data-callback");
                        timeout_window.close();
                    }
                });
                if let Ok(mut result) = fs::OpenOptions::new().append(true).open(&result_path) {
                    let _ = writeln!(result, "LOAD:{origin}/site-data");
                }
                cx.tasks.spawn(async move {
                    zgx::timer::sleep(std::time::Duration::from_secs(1)).await;
                    let clear_web = site_web.clone();
                    let before_path = result_path.clone();
                    let before_window = window.clone();
                    site_web.evaluate_js(
                        "(async()=>({local:localStorage.getItem('polyscale-site-data'),cookie:await fetch('/cookie-check').then(r=>r.text())}))()",
                        move |before| {
                            let _ = fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(&before_path)
                                .and_then(|mut file| writeln!(file, "BEFORE:{before:?}"));
                            if before.as_deref() != Ok(r#"{"cookie":"present","local":"present"}"#) {
                                let _ = fs::write(&before_path, format!("FAIL:before:{before:?}"));
                                callback_done.store(true, Ordering::Release);
                                before_window.close();
                                return;
                            }
                            let verify_web = clear_web.clone();
                            let verify_path = result_path.clone();
                            let verify_window = window.clone();
                            let clear_done = callback_done.clone();
                            clear_web.clear_site_data(&site_origin, move |cleared| {
                                let _ = fs::OpenOptions::new()
                                    .create(true)
                                    .append(true)
                                    .open(&verify_path)
                                    .and_then(|mut file| writeln!(file, "CLEAR:{cleared:?}"));
                                if let Err(error) = cleared {
                                    let _ = fs::write(verify_path, format!("FAIL:clear:{error}"));
                                    clear_done.store(true, Ordering::Release);
                                    verify_window.close();
                                    return;
                                }
                                verify_web.evaluate_js(
                                    "(async()=>({local:localStorage.getItem('polyscale-site-data'),cookie:await fetch('/cookie-check').then(r=>r.text())}))()",
                                    move |after| {
                                        let _ = fs::OpenOptions::new()
                                            .create(true)
                                            .append(true)
                                            .open(&verify_path)
                                            .and_then(|mut file| writeln!(file, "AFTER:{after:?}"));
                                        let success = after.as_deref()
                                            == Ok(r#"{"cookie":"absent","local":null}"#);
                                        let status = if success { "PASS" } else { "FAIL" };
                                        let _ = fs::write(verify_path, format!("{status}:{after:?}"));
                                        clear_done.store(true, Ordering::Release);
                                        verify_window.close();
                                    },
                                );
                            });
                        },
                    );
                });
                web.load_url(format!("{origin}/site-data"));
            } else if event_test {
                web.load_html(EVENT_FIXTURE, None);
                let web = web.clone();
                cx.tasks.spawn(async move {
                    zgx::timer::sleep(std::time::Duration::from_millis(300)).await;
                    web.load_html(EVENT_CHANGED_FIXTURE, None);
                });
            } else if eval_test {
                web.load_html(EVENT_CHANGED_FIXTURE, None);
                let eval_web = web.clone();
                let eval_path = eval_result_path.clone().expect("eval test has result path");
                let window = cx.window.clone();
                let eval_done = eval_done.clone();
                cx.tasks.spawn(async move {
                    zgx::timer::sleep(std::time::Duration::from_millis(800)).await;
                    run_evaluation_test(
                        eval_web,
                        eval_path.clone(),
                        window.clone(),
                        eval_done.clone(),
                    );
                    zgx::timer::sleep(std::time::Duration::from_secs(15)).await;
                    if !eval_done.swap(true, Ordering::AcqRel) {
                        let _ = fs::write(eval_path, "TIMEOUT");
                        window.close();
                    }
                });
            } else if download_test {
                web.load_url(origin);
                let download_page = web.clone();
                let download_window = cx.window.clone();
                let download_scene = cx.ui.scene.clone();
                let download_input = cx.ui.input.clone();
                let download_result = download_result_path
                    .clone()
                    .expect("download test has a result path");
                cx.tasks.spawn(async move {
                    let mut ready = false;
                    for _ in 0..200 {
                        if download_page.title() == "CEF OSR fixture"
                            && !download_page.state().with_untracked(|state| state.loading)
                        {
                            ready = true;
                            break;
                        }
                        zgx::timer::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    if !ready {
                        let _ = fs::write(&download_result, "TIMEOUT:fixture-page");
                        download_window.close();
                        return;
                    }

                    let click_result = download_result.clone();
                    let scene = download_scene.clone();
                    let input = download_input.clone();
                    download_page.evaluate_js(
                        "(()=>{const r=document.querySelector('#download').getBoundingClientRect();return {x:r.x+r.width/2,y:r.y+r.height/2}})()",
                        move |result| {
                            let position = result
                                .ok()
                                .and_then(|value| serde_json::from_str::<serde_json::Value>(&value).ok())
                                .and_then(|value| Some((value["x"].as_f64()? as f32, value["y"].as_f64()? as f32)));
                            let Some((x, y)) = position else {
                                let _ = fs::write(click_result, "FAIL:download-link-bounds");
                                return;
                            };
                            // Send a real ZGX pointer sequence through the
                            // normal WebView input path, so Chromium treats the
                            // attachment as a user-initiated download.
                            input.dispatch(&scene, InputEvent::PointerMove { x, y });
                            input.dispatch(&scene, InputEvent::PointerDown {
                                x,
                                y,
                                button: PointerButton::Primary,
                            });
                            input.dispatch(&scene, InputEvent::PointerUp {
                                x,
                                y,
                                button: PointerButton::Primary,
                            });
                        },
                    );
                    zgx::timer::sleep(std::time::Duration::from_secs(15)).await;
                    if !fs::read_to_string(&download_result)
                        .is_ok_and(|result| result == "PASS")
                    {
                        let _ = fs::write(&download_result, "TIMEOUT:download");
                        download_window.close();
                    }
                });
            } else {
                let fixture = FIXTURE.to_owned();
                web.load_html(&fixture, Some(&origin));
            }
            let page = web.clone();
            let visible = cx.ui.signal(true);
            let show = visible.clone();
            let hide_page = web.clone();
            let app = column()
                .w_full()
                .h_full()
                .gap(8.)
                .p(12.)
                .child(
                    row()
                        .gap(8.)
                        .child(
                            button()
                                .child(text("Reload"))
                                .on_click(move || page.reload()),
                        )
                        .child(button().child(text("Hide / show page")).on_click(move || {
                            show.update(|visible| *visible = !*visible);
                        })),
                )
                .child(
                    switch(
                        move || visible.get(),
                        move |_, _| zgx::web_view::web_view(&hide_page).grow().w_full().h_full(),
                    )
                    .grow()
                    .w_full()
                    .h_full(),
                );
            let app = if late_load_test {
                row()
                    .w_full()
                    .h_full()
                    .child(zgx::web_view::web_view(&web).grow())
                    .child(zgx::web_view::web_view(late_shared.as_ref().unwrap()).grow())
                    .child(zgx::web_view::web_view(late_isolated.as_ref().unwrap()).grow())
            } else if download_test {
                zgx::web_view::web_view(&web)
                    .grow()
                    .w_full()
                    .h_full()
            } else {
                app
            };
            let app = if event_subscription.is_some() {
                app.on_mount(move |handle| {
                    if let Some(subscription) = event_subscription {
                        handle.retain(subscription);
                    }
                })
            } else {
                app
            };
            let _view = cx.render(app);
        });
    let _ = fs::remove_dir_all(fixture_data_dir);
    run_result?;
    Ok(())
}

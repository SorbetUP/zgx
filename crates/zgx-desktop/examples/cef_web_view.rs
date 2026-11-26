// Local CEF OSR smoke page. Run from a CEF bundle so Chromium's framework,
// resources and sandbox helper are beside the app executable:
// `cargo run -p zgx-desktop --features cef --bin cef_web_view`.
// `ZGX_CEF_STRESS_RESULT=/tmp/cef-stress-result` runs 100 sequential browser
// create/close cycles in one runtime and writes callback counts to the result.
use std::{
    cell::RefCell,
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    rc::Rc,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    sync::{Arc, Mutex},
    thread,
};
use zgx::{
    compose::prelude::*,
    input::{DraggedText, InputDispatcher, InputEvent, Key, Modifiers, PointerButton},
    scene::Scene,
    web_view::{FindOptions, WebProfile, WebView, WebViewOptions},
};
use zgx_desktop::{Application, WindowBounds, WindowOptions};

const DOWNLOAD_PAYLOAD: &[u8] = b"cef-download-payload";
const CEF_STRESS_CYCLES: usize = 100;

async fn wait_for_diagnostic_count(
    diagnostic: &Path,
    prefix: &str,
    expected: usize,
    deadline: std::time::Instant,
) -> bool {
    loop {
        let count = fs::read_to_string(diagnostic)
            .unwrap_or_default()
            .lines()
            .filter(|line| line.starts_with(prefix))
            .count();
        if count >= expected {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        zgx::timer::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[cfg(debug_assertions)]
fn trace_close_race(result: Option<&Path>, message: &str) {
    let Some(result) = result else { return };
    let (Some(parent), Ok(temp_dir)) = (result.parent(), std::env::temp_dir().canonicalize())
    else {
        return;
    };
    if !parent
        .canonicalize()
        .is_ok_and(|parent| parent.starts_with(temp_dir))
    {
        return;
    }
    let diagnostic = result.with_extension("diagnostic");
    if fs::metadata(&diagnostic).is_ok_and(|metadata| metadata.len() >= 32 * 1024) {
        return;
    }
    let message: String = message.chars().take(240).collect();
    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(diagnostic)
        .and_then(|mut file| writeln!(file, "{message}"));
}

#[cfg(not(debug_assertions))]
#[inline(always)]
fn trace_close_race(_: Option<&Path>, _: &str) {}

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

const POLICY_FIXTURE: &str = r#"<!doctype html><meta charset=utf-8><title>policy</title>
<a id="allowed" href="/policy-allowed">allowed</a>
<a id="denied" href="/policy-denied">denied</a>
"#;

// Sandbox-origin fixture: the parent is served over http, but the iframe is
// `sandbox="allow-scripts"` (no allow-same-origin), so its security origin is
// OPAQUE ("null") even though its URL is http. Its message must arrive with
// origin "null", never with the server's http origin.
const SANDBOX_PARENT_FIXTURE: &str = r#"<!doctype html>
<meta charset="utf-8"><title>sandbox parent</title>
<iframe sandbox="allow-scripts" src="/sandbox-child"></iframe>
<script>window.webkit.messageHandlers.zgx.postMessage({source:"sandbox-parent"})</script>
"#;

const SANDBOX_CHILD_FIXTURE: &str = r#"<!doctype html>
<meta charset="utf-8"><title>sandbox child</title>
<script>window.webkit.messageHandlers.zgx.postMessage({source:"sandbox-child"})</script>
"#;

const POPUP_PARENT_FIXTURE: &str = r#"<!doctype html><meta charset=utf-8><title>popup parent</title>
<script>
window.__popupMessages=[];
fetch('/popup-bridge-report?zgx='+encodeURIComponent(typeof window.zgx)+'&post='+encodeURIComponent(typeof window.zgx?.postMessage));
window.addEventListener('message', event => {
  window.__popupMessages.push({origin:event.origin,data:event.data});
  fetch('/popup-parent-report?origin='+encodeURIComponent(event.origin)+'&data='+encodeURIComponent(JSON.stringify(event.data)));
  try { window.zgx.postMessage({source:'popup-parent',ok:event.origin===location.origin&&event.data?.ok===true,eventOrigin:event.origin,data:event.data,hasSource:!!event.source}); }
  catch (error) { fetch('/popup-bridge-error?error='+encodeURIComponent(String(error))+'&zgx='+encodeURIComponent(typeof window.zgx)); }
});
function openFixturePopup(){window.__popupClicked=(window.__popupClicked||0)+1;window.open('/popup-child', 'fixture-popup');}
</script><button id="open-popup" onclick="openFixturePopup()">Open fixture popup</button>
"#;

const POPUP_CHILD_FIXTURE: &str = r#"<!doctype html><meta charset=utf-8><title>popup child</title>
<script>
const ok=!!window.opener&&document.cookie.includes('popup_cookie=shared');
fetch('/popup-report?opener='+Number(!!window.opener)+'&cookie='+encodeURIComponent(document.cookie));
if(window.opener)window.opener.postMessage({ok,hasOpener:!!window.opener,cookie:document.cookie},'*');
setTimeout(()=>window.close(),1000);
</script><p>Completing popup fixture</p>
"#;

const DRAG_SOURCE_FIXTURE: &str = r#"<!doctype html><meta charset=utf-8><title>drag source</title>
<style>html,body{margin:0;width:100%;height:100%;font:24px system-ui}body{display:grid;place-items:center}a{display:inline-block;padding:24px;background:#d5e9ff}</style>
<a id="drag-link" draggable="true" href="/drag-linked">Drag this link</a>
"#;

const DRAG_TARGET_FIXTURE: &str = r#"<!doctype html><meta charset=utf-8><title>drag target</title>
<style>html,body{margin:0;width:100%;height:100%;font:24px system-ui}#drop{width:100%;height:100%;display:grid;place-items:center;border:4px dashed #579;box-sizing:border-box}</style>
<div id="drop">Drop here</div>
<script>
const target=document.querySelector('#drop');
target.addEventListener('dragover',event=>event.preventDefault());
target.addEventListener('drop',event=>{
  event.preventDefault();
  const html=event.dataTransfer.getData('text/html');
  const link=new DOMParser().parseFromString(html,'text/html').querySelector('a')?.href||'';
  const files=Array.from(event.dataTransfer.files).map(file=>({name:file.name,size:file.size}));
  window.zgx.postMessage({source:'drag-target',uri:event.dataTransfer.getData('text/uri-list'),plain:event.dataTransfer.getData('text/plain'),htmlLink:link,files});
});
</script>
"#;

type FixtureRect = Rc<RefCell<Option<zgx::compose::NodeHandle>>>;

fn capture_fixture_rect(handle: zgx::compose::NodeHandle, output: FixtureRect) {
    *output.borrow_mut() = Some(handle);
}

fn fixture_rect(output: &FixtureRect) -> Option<(f32, f32, f32, f32)> {
    let handle = output.borrow().clone()?;
    let rect = handle.window_bounds()?;
    (rect.width > 0. && rect.height > 0.).then_some((rect.x, rect.y, rect.width, rect.height))
}

async fn perform_drag(
    input: &InputDispatcher,
    scene: &Rc<RefCell<Scene>>,
    from: (f32, f32),
    to: (f32, f32),
) {
    let moved = input.dispatch(
        scene,
        InputEvent::PointerMove {
            x: from.0,
            y: from.1,
        },
    );
    let down = input.dispatch(
        scene,
        InputEvent::PointerDown {
            x: from.0,
            y: from.1,
            button: PointerButton::Primary,
        },
    );
    let _ = (moved, down);
    zgx::timer::sleep(std::time::Duration::from_millis(40)).await;
    let threshold = (from.0 + 12.0, from.1 + 8.0);
    let threshold_result = input.dispatch(
        scene,
        InputEvent::PointerMove {
            x: threshold.0,
            y: threshold.1,
        },
    );
    let _ = threshold_result;
    zgx::timer::sleep(std::time::Duration::from_millis(120)).await;
    for step in 1..=12 {
        let fraction = step as f32 / 12.0;
        let result = input.dispatch(
            scene,
            InputEvent::PointerMove {
                x: threshold.0 + (to.0 - threshold.0) * fraction,
                y: threshold.1 + (to.1 - threshold.1) * fraction,
            },
        );
        if step == 12 {
            let _ = result;
        }
        zgx::timer::sleep(std::time::Duration::from_millis(30)).await;
    }
    input.dispatch(
        scene,
        InputEvent::PointerUp {
            x: to.0,
            y: to.1,
            button: PointerButton::Primary,
        },
    );
}

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

fn append_result(path: &Path, line: &str) {
    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| writeln!(file, "{line}"));
}

async fn wait_for_result_line(path: &Path, needle: &str, deadline: std::time::Instant) -> bool {
    loop {
        if fs::read_to_string(path)
            .unwrap_or_default()
            .contains(needle)
        {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        zgx::timer::sleep(std::time::Duration::from_millis(100)).await;
    }
}

async fn wait_for_title(web: &WebView, prefix: &str, deadline: std::time::Instant) -> String {
    loop {
        let title = web.title();
        if title.starts_with(prefix) {
            return title;
        }
        if std::time::Instant::now() >= deadline {
            return title;
        }
        zgx::timer::sleep(std::time::Duration::from_millis(100)).await;
    }
}

fn start_fixture_server(
    download_test: bool,
    site_data_test: bool,
    site_data_frame: bool,
    popup_test: bool,
    drag_test: bool,
    sandbox_test: bool,
    html_base_result_path: Option<std::path::PathBuf>,
    fixed_port: Option<u16>,
) -> Result<String, Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(("127.0.0.1", fixed_port.unwrap_or(0)))?;
    let address = listener.local_addr()?;
    let origin = format!("http://{address}");
    // A subdomain-style sibling of `localhost` (all of *.localhost is
    // loopback), served by the same listener, to stand in for two same-site
    // hosts such as example.com and app.example.com.
    let sibling_origin = format!("http://sub.localhost:{}", address.port());
    let result_path = std::env::var_os("ZGX_CEF_DOWNLOAD_RESULT").map(std::path::PathBuf::from);
    let site_data_result_path =
        std::env::var_os("ZGX_CEF_SITE_DATA_RESULT").map(std::path::PathBuf::from);
    let popup_diagnostic_path = std::env::var_os("ZGX_CEF_POPUP_RESULT")
        .map(std::path::PathBuf::from)
        .map(|path| path.with_extension("diagnostic"));
    let drag_diagnostic_path = std::env::var_os("ZGX_CEF_DRAG_RESULT")
        .map(std::path::PathBuf::from)
        .map(|path| path.with_extension("diagnostic"));
    let html_base_log_path = html_base_result_path;
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let result_path = result_path.clone();
            let site_data_result_path = site_data_result_path.clone();
            let popup_diagnostic_path = popup_diagnostic_path.clone();
            let drag_diagnostic_path = drag_diagnostic_path.clone();
            let html_base_log_path = html_base_log_path.clone();
            let sibling_origin = sibling_origin.clone();
            thread::spawn(move || {
                let mut stream = stream;
                let mut request = [0; 2048];
                let count = stream.read(&mut request).unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..count]);
                let path = request.split_whitespace().nth(1).unwrap_or("/");
                if let Some(path_out) = popup_diagnostic_path.as_ref() {
                    let _ = fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path_out)
                        .and_then(|mut result| writeln!(result, "POPUP_HTTP:{path}"));
                }
                if let Some(path_out) = drag_diagnostic_path.as_ref() {
                    let _ = fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path_out)
                        .and_then(|mut result| writeln!(result, "DRAG_HTTP:{path}"));
                }
                if let Some(result_path) = html_base_log_path.as_ref()
                    && let Ok(mut result) = fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(result_path)
                {
                    let _ = writeln!(result, "HTTP:{path}");
                }
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
                // Site-data fixture pages that depend on the request (a
                // domain cookie only for *.localhost hosts) or on the run
                // phase (a subframe embedding the sibling origin) are built
                // here, before the static chain below.
                if site_data_test
                    && matches!(
                        path,
                        "/site-data" | "/site-data-frame" | "/cookie-check-all" | "/verify"
                    )
                {
                    let host_is_localhost = request
                        .lines()
                        .find(|line| line.to_ascii_lowercase().starts_with("host:"))
                        .is_some_and(|line| line.contains("localhost"));
                    // Only the data pages set cookies; the read-only pages
                    // (/verify, /cookie-check-all) must not, or they would
                    // re-create what the clear is expected to remove.
                    let sets_cookies = matches!(path, "/site-data" | "/site-data-frame");
                    let mut headers = String::new();
                    if sets_cookies {
                        headers.push_str(
                            "Set-Cookie: fixture_cookie=connected; HttpOnly; SameSite=Lax\r\n",
                        );
                        if host_is_localhost {
                            // A Domain=.localhost cookie, shared by the site's
                            // subdomains: the record a per-origin storage clear
                            // alone can miss.
                            headers.push_str(
                                "Set-Cookie: site_domain_cookie=shared; Domain=localhost; Path=/; SameSite=Lax\r\n",
                            );
                        }
                    }
                    let (body, extra_headers) = if path == "/site-data" {
                        let frame = if site_data_frame {
                            format!("<iframe src=\"{sibling_origin}/site-data-frame\"></iframe>")
                        } else {
                            String::new()
                        };
                        (
                            format!(
                                "<!doctype html><meta charset=utf-8><title>site-data</title><script>localStorage.setItem('polyscale-site-data','present')</script>{frame}"
                            ),
                            String::new(),
                        )
                    } else if path == "/site-data-frame" {
                        (
                            "<!doctype html><meta charset=utf-8><title>site-data frame</title><script>localStorage.setItem('polyscale-site-data','present')</script>".to_owned(),
                            String::new(),
                        )
                    } else if path == "/cookie-check-all" {
                        let cookie_header = request
                            .lines()
                            .find(|line| line.to_ascii_lowercase().starts_with("cookie:"))
                            .unwrap_or("");
                        let fixture = if cookie_header.contains("fixture_cookie=connected") {
                            "present"
                        } else {
                            "absent"
                        };
                        let domain = if cookie_header.contains("site_domain_cookie=shared") {
                            "present"
                        } else {
                            "absent"
                        };
                        (format!("fixture={fixture};domain={domain}"), String::new())
                    } else {
                        (
                            "<!doctype html><meta charset=utf-8><title>verify</title><script>(async()=>{const c=await fetch('/cookie-check-all').then(r=>r.text());document.title='cookie:'+c+'|local:'+localStorage.getItem('polyscale-site-data');})()</script>".to_owned(),
                            String::new(),
                        )
                    };
                    let _ = extra_headers;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.write_all(body.as_bytes());
                    return;
                }
                let (status, content_type, headers, body) = if popup_test && path == "/popup-parent"
                {
                    (
                        "200 OK",
                        "text/html; charset=utf-8",
                        "Set-Cookie: popup_cookie=shared; SameSite=Lax\r\n",
                        POPUP_PARENT_FIXTURE.as_bytes(),
                    )
                } else if popup_test && path == "/popup-child" {
                    (
                        "200 OK",
                        "text/html; charset=utf-8",
                        "",
                        POPUP_CHILD_FIXTURE.as_bytes(),
                    )
                } else if drag_test && path == "/drag-source" {
                    (
                        "200 OK",
                        "text/html; charset=utf-8",
                        "",
                        DRAG_SOURCE_FIXTURE.as_bytes(),
                    )
                } else if drag_test && path == "/drag-target" {
                    (
                        "200 OK",
                        "text/html; charset=utf-8",
                        "",
                        DRAG_TARGET_FIXTURE.as_bytes(),
                    )
                } else if drag_test && path == "/drag-linked" {
                    (
                        "200 OK",
                        "text/plain; charset=utf-8",
                        "",
                        b"drag-link-destination".as_slice(),
                    )
                } else if site_data_test && path == "/cookie-check" {
                    let has_cookie = request.lines().any(|line| {
                        line.to_ascii_lowercase().starts_with("cookie:")
                            && line.contains("fixture_cookie=connected")
                    });
                    let body: &'static [u8] = if has_cookie { b"present" } else { b"absent" };
                    ("200 OK", "text/plain", "", body)
                } else if path == "/policy" {
                    (
                        "200 OK",
                        "text/html; charset=utf-8",
                        "",
                        POLICY_FIXTURE.as_bytes(),
                    )
                } else if path == "/policy-allowed" {
                    (
                        "200 OK",
                        "text/html; charset=utf-8",
                        "",
                        b"<!doctype html><meta charset=utf-8><title>policy allowed</title>"
                            .as_slice(),
                    )
                } else if path == "/policy-denied" {
                    (
                        "200 OK",
                        "text/html; charset=utf-8",
                        "",
                        b"<!doctype html><meta charset=utf-8><title>policy denied</title>"
                            .as_slice(),
                    )
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
                } else if sandbox_test && path == "/sandbox-parent" {
                    (
                        "200 OK",
                        "text/html; charset=utf-8",
                        "",
                        SANDBOX_PARENT_FIXTURE.as_bytes(),
                    )
                } else if sandbox_test && path == "/sandbox-child" {
                    (
                        "200 OK",
                        "text/html; charset=utf-8",
                        "",
                        SANDBOX_CHILD_FIXTURE.as_bytes(),
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
    // launches do not prompt for "Chromium Safe Storage" access. A data dir
    // set from outside is kept (and not removed on exit), so a "restart"
    // scenario can run two fixture processes over the same profile directory.
    let external_data_dir = std::env::var_os("POLYSCALE_CEF_DATA_DIR");
    let fixture_data_dir = external_data_dir
        .clone()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("zgx-cef-fixture-{}", std::process::id()))
        });
    unsafe {
        std::env::set_var("ZGX_CEF_TEST_FIXTURE", "1");
        std::env::set_var("POLYSCALE_CEF_DATA_DIR", &fixture_data_dir);
    }
    if external_data_dir.is_some() {
        // Create it up front so the mock-keychain fixture check (which
        // canonicalizes the data dir) accepts it from the first launch.
        let _ = fs::create_dir_all(&fixture_data_dir);
    }
    let download_result_path =
        std::env::var_os("ZGX_CEF_DOWNLOAD_RESULT").map(std::path::PathBuf::from);
    let download_test = download_result_path.is_some();
    let input_result_path = std::env::args()
        .find_map(|argument| {
            argument
                .strip_prefix("--cef-input-result=")
                .map(std::path::PathBuf::from)
        })
        .or_else(|| std::env::var_os("ZGX_CEF_INPUT_RESULT").map(std::path::PathBuf::from));
    let skip_input = std::env::var_os("ZGX_CEF_SKIP_INPUT").is_some()
        || std::env::args().any(|argument| argument == "--cef-skip-input");
    let skip_ime = std::env::var_os("ZGX_CEF_SKIP_IME").is_some()
        || std::env::args().any(|argument| argument == "--cef-skip-ime");
    let input_test = input_result_path.is_some();
    let event_result_path = std::env::var_os("ZGX_CEF_EVENT_RESULT").map(std::path::PathBuf::from);
    let event_test = event_result_path.is_some();
    let sandbox_origin_result_path =
        std::env::var_os("ZGX_CEF_SANDBOX_ORIGIN_RESULT").map(std::path::PathBuf::from);
    let sandbox_origin_test = sandbox_origin_result_path.is_some();
    let eval_result_path = std::env::var_os("ZGX_CEF_EVAL_RESULT").map(std::path::PathBuf::from);
    let eval_test = eval_result_path.is_some();
    let html_base_result_path = std::env::args()
        .find_map(|argument| {
            argument
                .strip_prefix("--cef-html-base-result=")
                .map(std::path::PathBuf::from)
        })
        .or_else(|| std::env::var_os("ZGX_CEF_HTML_BASE_RESULT").map(std::path::PathBuf::from));
    if let Some(path) = html_base_result_path.as_ref() {
        unsafe { std::env::set_var("ZGX_CEF_HTML_BASE_RESULT", path) };
    }
    let html_base_test = html_base_result_path.is_some();
    let site_data_result_path =
        std::env::var_os("ZGX_CEF_SITE_DATA_RESULT").map(std::path::PathBuf::from);
    let site_data_test = site_data_result_path.is_some();
    // "record" and "verify" run the site-data fixture as two processes over
    // the same POLYSCALE_CEF_DATA_DIR: a simulated restart that must still
    // let clear_site_data find the site's origins (and clear a
    // subdomain-style sibling origin) from the persisted registry.
    let site_data_phase = std::env::var("ZGX_CEF_SITE_DATA_PHASE").unwrap_or_default();
    let late_load_result_path =
        std::env::var_os("ZGX_CEF_LATE_LOAD_RESULT").map(std::path::PathBuf::from);
    let late_load_test = late_load_result_path.is_some();
    let close_race_result_path =
        std::env::var_os("ZGX_CEF_CLOSE_RACE_RESULT").map(std::path::PathBuf::from);
    let close_race_test = close_race_result_path.is_some();
    let stress_result_path =
        std::env::var_os("ZGX_CEF_STRESS_RESULT").map(std::path::PathBuf::from);
    let stress_test = stress_result_path.is_some();
    let navigation_eval_race_result_path =
        std::env::var_os("ZGX_CEF_NAV_EVAL_RACE_RESULT").map(std::path::PathBuf::from);
    let navigation_eval_race_test = navigation_eval_race_result_path.is_some();
    let popup_result_path = std::env::var_os("ZGX_CEF_POPUP_RESULT").map(std::path::PathBuf::from);
    let popup_test = popup_result_path.is_some();
    let popup_close_race_result_path =
        std::env::var_os("ZGX_CEF_POPUP_CLOSE_RACE_RESULT").map(std::path::PathBuf::from);
    let popup_close_race_test = popup_close_race_result_path.is_some();
    let policy_result_path =
        std::env::var_os("ZGX_CEF_POLICY_RESULT").map(std::path::PathBuf::from);
    let policy_test = policy_result_path.is_some();
    let drag_result_path = std::env::var_os("ZGX_CEF_DRAG_RESULT").map(std::path::PathBuf::from);
    let drag_test = drag_result_path.is_some();
    let drag_fixture_file = fixture_data_dir.join("drag-payload.txt");
    if drag_test {
        fs::create_dir_all(&fixture_data_dir)?;
        fs::write(&drag_fixture_file, b"cef-native-file-drag")?;
    }
    let automated = download_test
        || input_test
        || event_test
        || sandbox_origin_test
        || eval_test
        || html_base_test
        || site_data_test
        || late_load_test
        || close_race_test
        || stress_test
        || navigation_eval_race_test
        || popup_test
        || popup_close_race_test
        || drag_test
        || policy_test;
    if let Some(result_path) = download_result_path.as_ref() {
        fs::write(result_path, format!("START:auto={automated}\n"))?;
    }
    if let Some(result_path) = event_result_path.as_ref() {
        fs::write(result_path, "START\n")?;
    }
    if let Some(result_path) = sandbox_origin_result_path.as_ref() {
        fs::write(result_path, "START\n")?;
    }
    if let Some(result_path) = input_result_path.as_ref() {
        fs::write(result_path, "START\n")?;
    }
    if let Some(result_path) = eval_result_path.as_ref() {
        fs::write(result_path, "START\n")?;
    }
    if let Some(result_path) = html_base_result_path.as_ref() {
        fs::write(result_path, "START\n")?;
    }
    if let Some(result_path) = site_data_result_path.as_ref() {
        fs::write(result_path, "START\n")?;
    }
    if let Some(result_path) = close_race_result_path.as_ref() {
        fs::write(result_path, "START\n")?;
        trace_close_race(Some(result_path), "FIXTURE_START");
    }
    if let Some(result_path) = stress_result_path.as_ref() {
        fs::write(result_path, "START\n")?;
        trace_close_race(Some(result_path), "STRESS_FIXTURE_START");
    }
    if let Some(result_path) = navigation_eval_race_result_path.as_ref() {
        fs::write(result_path, "START\n")?;
    }
    if let Some(result_path) = popup_result_path.as_ref() {
        fs::write(result_path, "START\n")?;
    }
    if let Some(result_path) = popup_close_race_result_path.as_ref() {
        fs::write(result_path, "START\n")?;
        trace_close_race(Some(result_path), "POPUP_CLOSE_RACE_FIXTURE_START");
    }
    if let Some(result_path) = drag_result_path.as_ref() {
        fs::write(result_path, "START\n")?;
    }
    if let Some(result_path) = policy_result_path.as_ref() {
        fs::write(result_path, "START\n")?;
    }
    // The restart scenario runs two fixture processes that must serve the
    // site on the same port, so both see the same origins: the record
    // process leaves its port beside the result file and the verify process
    // binds it again.
    let site_data_port_file = site_data_result_path
        .as_ref()
        .map(|path| path.with_extension("port"));
    let fixed_site_data_port = if site_data_phase == "verify" {
        site_data_port_file
            .as_ref()
            .and_then(|path| fs::read_to_string(path).ok())
            .and_then(|text| text.trim().parse::<u16>().ok())
    } else {
        None
    };
    let origin = start_fixture_server(
        download_test,
        site_data_test || late_load_test,
        site_data_test && site_data_phase == "record",
        popup_test || popup_close_race_test,
        drag_test,
        sandbox_origin_test,
        html_base_result_path.clone(),
        fixed_site_data_port,
    )?;
    if site_data_test
        && site_data_phase == "record"
        && let Some(port_file) = &site_data_port_file
    {
        let _ = fs::write(
            port_file,
            format!("{}\n", origin.rsplit(':').next().unwrap_or_default()),
        );
    }
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
            if let Some(result_path) = input_result_path.as_ref() {
                let result_path = result_path.clone();
                cx.on_closed(move || {
                    trace_close_race(Some(&result_path), "INPUT_WINDOW_CLOSED");
                });
            }
            let profile = if drag_test {
                WebProfile::Named("zgx-drag-integration-fixture".into())
            } else if late_load_test {
                WebProfile::Named("zgx-late-load-fixture".into())
            } else if site_data_test {
                // Persistent, so the restart scenario (record then verify
                // over the same data dir) exercises the on-disk registry.
                WebProfile::Default
            } else {
                WebProfile::Ephemeral
            };
            let web = WebView::new(
                &cx.ui.runtime,
                WebViewOptions {
                    profile,
                    new_windows: popup_test || popup_close_race_test,
                    // The event fixture asserts page messages from both the
                    // main frame and a srcdoc iframe, so its bridge must be on.
                    // The sandbox fixture asserts the sandboxed iframe's
                    // message arrives with origin "null".
                    messages: drag_test || popup_test || event_test || sandbox_origin_test,
                    ..Default::default()
                },
            );
            let stress_pages: Vec<WebView> = if stress_test {
                (0..CEF_STRESS_CYCLES)
                    .map(|cycle| {
                        let page = WebView::new(
                            &cx.ui.runtime,
                            WebViewOptions {
                                profile: WebProfile::Ephemeral,
                                ..Default::default()
                            },
                        );
                        page.load_html(
                            format!(
                                "<!doctype html><meta charset=utf-8><title>CEF cycle {cycle}</title><body>CEF cycle {cycle}</body>"
                            ),
                            None,
                        );
                        page
                    })
                    .collect()
            } else {
                Vec::new()
            };
            trace_close_race(close_race_result_path.as_deref(), "WEBVIEW_CONTROLLER_CREATED");
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
            let drag_target_web = drag_test.then(|| {
                WebView::new(
                    &cx.ui.runtime,
                    WebViewOptions {
                        profile: WebProfile::Named("zgx-drag-integration-fixture".into()),
                        messages: true,
                        ..Default::default()
                    },
                )
            });
            let source_rect = Rc::new(RefCell::new(None));
            let file_source_rect = Rc::new(RefCell::new(None));
            let target_rect = Rc::new(RefCell::new(None));
            let drag_file_path = drag_fixture_file.clone();
            let drag_file_source = div()
                .w(290.)
                .h_full()
                .child(text("CEF file drag source"))
                .on_drag(move || {
                    DraggedText::with_link_and_files(
                        "",
                        None,
                        None,
                        vec![drag_file_path.clone()],
                    )
                })
                .on_mount({
                    let output = file_source_rect.clone();
                    move |handle| capture_fixture_rect(handle, output.clone())
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
            let event_subscription = event_result_path.clone().map(|result_path| {
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
            // Sandbox-origin fixture: a `sandbox="allow-scripts"` iframe is
            // served over http, so frame.url() would say the http origin —
            // but its security origin is opaque and must arrive as "null".
            let sandbox_subscription = sandbox_origin_result_path.clone().map(|result_path| {
                let observed = Arc::new(Mutex::new((false, false)));
                let event_state = observed.clone();
                let event_done = Arc::new(AtomicBool::new(false));
                let completed = event_done.clone();
                let log_path = result_path.clone();
                let write_path = result_path.clone();
                let window = cx.window.clone();
                let expected_origin = origin.clone();
                let subscription = web.on_event(move |event| {
                    let zgx::web_view::WebViewEvent::Message {
                        body,
                        origin: message_origin,
                        main_frame,
                    } = event
                    else {
                        return;
                    };
                    if let Ok(mut log) = fs::OpenOptions::new().append(true).open(&log_path) {
                        let _ = writeln!(log, "SANDBOX_EVENT:{body}:{message_origin}:{main_frame}");
                    }
                    let mut observed = event_state
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner());
                    match body.as_str() {
                        r#"{"source":"sandbox-child"}"# => {
                            // The sandboxed iframe has an opaque origin even
                            // though its URL is http: it must arrive as
                            // "null", never as the fixture server's origin.
                            if message_origin == "null" {
                                observed.0 = true;
                            } else {
                                drop(observed);
                                let _ = fs::write(
                                    &write_path,
                                    format!("FAIL:sandbox-child-origin:{message_origin}"),
                                );
                                window.close();
                                return;
                            }
                        }
                        r#"{"source":"sandbox-parent"}"# => {
                            if message_origin == &expected_origin {
                                observed.1 = true;
                            }
                        }
                        _ => {}
                    }
                    if observed.0 && observed.1 {
                        event_done.store(true, Ordering::Release);
                        let _ = fs::write(&write_path, "PASS");
                        window.close();
                    }
                });
                let timeout_window = cx.window.clone();
                let timeout_path = result_path;
                cx.tasks.spawn(async move {
                    zgx::timer::sleep(std::time::Duration::from_secs(15)).await;
                    if !completed.load(Ordering::Acquire) {
                        let _ = fs::write(timeout_path, "TIMEOUT");
                        timeout_window.close();
                    }
                });
                subscription
            });
            let eval_done = Arc::new(AtomicBool::new(false));
            let mut popup_subscription = None;
            let mut drag_subscription = None;
            if drag_test {
                let target = drag_target_web
                    .as_ref()
                    .expect("drag target WebView exists")
                    .clone();
                let result_path = drag_result_path
                    .clone()
                    .expect("drag integration result path exists");
                let expected_link = format!("{origin}/drag-linked");
                let expected_file = fixture_data_dir.join("drag-payload.txt");
                let expected_file_size = fs::metadata(&expected_file)
                    .map(|metadata| metadata.len())
                    .unwrap_or_default();
                let file_received = Arc::new(AtomicBool::new(false));
                let link_received = Arc::new(AtomicBool::new(false));
                let event_done = Arc::new(AtomicBool::new(false));
                let callback_file_received = file_received.clone();
                let callback_link_received = link_received.clone();
                let callback_done = event_done.clone();
                let callback_path = result_path.clone();
                let callback_link = expected_link.clone();
                let callback_name = expected_file
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let callback_window = cx.window.clone();
                let target_origin = origin.clone();
                let subscription = target.on_event(move |event| {
                    let zgx::web_view::WebViewEvent::Message {
                        body,
                        origin,
                        main_frame: true,
                    } = event
                    else {
                        return;
                    };
                    if origin != &target_origin {
                        return;
                    }
                    let Ok(message) = serde_json::from_str::<serde_json::Value>(body) else {
                        return;
                    };
                    if message["source"] != "drag-target" {
                        return;
                    }
                    let files = message["files"].as_array().cloned().unwrap_or_default();
                    if !files.is_empty() {
                        let passed = files.iter().any(|file| {
                            file["name"] == callback_name
                                && file["size"].as_u64() == Some(expected_file_size)
                        });
                        callback_file_received.store(passed, Ordering::Release);
                        let _ = fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&callback_path)
                            .and_then(|mut result| writeln!(result, "FILE_DROP:{message}:pass={passed}"));
                    } else {
                        let uri = message["uri"].as_str().unwrap_or_default().trim();
                        let html_link = message["htmlLink"].as_str().unwrap_or_default();
                        let passed = uri == callback_link || html_link == callback_link;
                        callback_link_received.store(passed, Ordering::Release);
                        let _ = fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&callback_path)
                            .and_then(|mut result| writeln!(result, "LINK_DROP:{message}:pass={passed}"));
                    }
                    if callback_file_received.load(Ordering::Acquire)
                        && callback_link_received.load(Ordering::Acquire)
                        && !callback_done.swap(true, Ordering::AcqRel)
                    {
                        let _ = fs::write(&callback_path, "PASS");
                        callback_window.close();
                    }
                });
                drag_subscription = Some(subscription);

                let source = web.clone();
                let task_target = target.clone();
                let input = cx.ui.input.clone();
                let scene = cx.ui.scene.clone();
                let task_source_rect = source_rect.clone();
                let task_file_source_rect = file_source_rect.clone();
                let task_target_rect = target_rect.clone();
                let window = cx.window.clone();
                let timeout_result = result_path.clone();
                let timeout_done = event_done.clone();
                cx.tasks.spawn(async move {
                    source.load_url(format!("{origin}/drag-source"));
                    task_target.load_url(format!("{origin}/drag-target"));
                    let mut ready = false;
                    for _ in 0..200 {
                        if source.title() == "drag source"
                            && task_target.title() == "drag target"
                            && !source.state().with_untracked(|state| state.loading)
                            && !task_target.state().with_untracked(|state| state.loading)
                        {
                            ready = true;
                            break;
                        }
                        zgx::timer::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    if !ready {
                        let _ = fs::write(
                            &timeout_result,
                            format!(
                                "FAIL:drag-pages-not-ready:source_title={:?}:source_loading={}:target_title={:?}:target_loading={}",
                                source.title(),
                                source.state().with_untracked(|state| state.loading),
                                task_target.title(),
                                task_target.state().with_untracked(|state| state.loading),
                            ),
                        );
                        window.close();
                        return;
                    }
                    let mut fixture_rects = None;
                    for _ in 0..100 {
                        let source = fixture_rect(&task_source_rect);
                        let file = fixture_rect(&task_file_source_rect);
                        let target = fixture_rect(&task_target_rect);
                        if let (Some(source), Some(file), Some(target)) = (source, file, target) {
                            fixture_rects = Some((source, file, target));
                            break;
                        }
                        zgx::timer::sleep(std::time::Duration::from_millis(20)).await;
                    }
                    let Some((source, file_source, target)) = fixture_rects else {
                        let _ = fs::write(&timeout_result, "FAIL:fixture-rects-not-mounted");
                        window.close();
                        return;
                    };
                    let center = |(x, y, width, height): (f32, f32, f32, f32)| {
                        (x + width / 2., y + height / 2.)
                    };
                    let source_link = center(source);
                    let file_source = center(file_source);
                    let target_drop = center(target);

                    // First inject a typed file payload through ZGX's normal
                    // drag dispatcher into the target's CEF DragTarget API.
                    // The URL phase below uses Chromium's real StartDragging
                    // callback as its source.
                    // The file payload begins at the center of the actual
                    // middle-column ZGX drag-source card.
                    perform_drag(&input, &scene, file_source, target_drop).await;
                    let mut file_ok = false;
                    for _ in 0..100 {
                        if file_received.load(Ordering::Acquire) {
                            file_ok = true;
                            break;
                        }
                        zgx::timer::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    if !file_ok {
                        let _ = fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&timeout_result)
                            .and_then(|mut result| writeln!(result, "FILE_DROP_NOT_RECEIVED"));
                    }

                    // Now drag a real link from the source Chromium page to
                    // the second Chromium page, preserving the URI metadata.
                    perform_drag(&input, &scene, source_link, target_drop).await;
                    for _ in 0..120 {
                        if event_done.load(Ordering::Acquire) {
                            return;
                        }
                        zgx::timer::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    if !timeout_done.swap(true, Ordering::AcqRel) {
                        let status = if file_received.load(Ordering::Acquire)
                            && link_received.load(Ordering::Acquire)
                        {
                            "PASS"
                        } else {
                            "FAIL:drag-drop-incomplete"
                        };
                        let _ = fs::write(&timeout_result, status);
                        window.close();
                    }
                });
            } else if navigation_eval_race_test {
                let page = web.clone();
                let navigation = web.clone();
                let result_path = navigation_eval_race_result_path
                    .clone()
                    .expect("navigation-evaluation race result path exists");
                let window = cx.window.clone();
                let done = Arc::new(AtomicBool::new(false));
                let callback_done = done.clone();
                web.load_html(EVENT_FIXTURE, None);
                cx.tasks.spawn(async move {
                    for _ in 0..100 {
                        if page.title() == "CEF event initial"
                            && !page.state().with_untracked(|state| state.loading)
                        {
                            break;
                        }
                        zgx::timer::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    if page.title() != "CEF event initial" {
                        let _ = fs::write(&result_path, "FAIL:initial-navigation");
                        window.close();
                        return;
                    }
                    let result_path_callback = result_path.clone();
                    let callback_window = window.clone();
                    page.evaluate_js(
                        "new Promise(resolve => setTimeout(() => resolve('late'), 500))",
                        move |result| {
                            let passed = result.as_ref().err().is_some_and(|error| {
                                error.contains("superseded by a navigation")
                            });
                            let status = if passed {
                                "PASS"
                            } else {
                                "FAIL:stale-evaluation-result"
                            };
                            let _ = fs::write(result_path_callback, status);
                            callback_done.store(true, Ordering::Release);
                            callback_window.close();
                        },
                    );
                    zgx::timer::sleep(std::time::Duration::from_millis(25)).await;
                    navigation.load_html(EVENT_CHANGED_FIXTURE, None);
                    zgx::timer::sleep(std::time::Duration::from_secs(6)).await;
                    if !done.swap(true, Ordering::AcqRel) {
                        let _ = fs::write(result_path, "TIMEOUT:evaluation-callback");
                        window.close();
                    }
                });
            } else if late_load_test {
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
            } else if close_race_test {
                // The backend closes this fixture controller immediately
                // after CEF accepts asynchronous creation; mounting closes
                // the window while the late creation is still pending.
            } else if site_data_test {
                let site_data_port = origin.rsplit(':').next().unwrap_or_default().to_owned();
                let localhost_origin = format!("http://localhost:{site_data_port}");
                // A subdomain-style sibling of localhost (all of *.localhost
                // is loopback, served by the same listener): together with
                // localhost it simulates two same-site hosts.
                let sibling_origin = format!("http://sub.localhost:{site_data_port}");
                if site_data_phase == "record" {
                    // Restart scenario, process 1: visit the site on both of
                    // its hosts — the sibling only through a subframe, which
                    // is what records its origin — then exit, leaving the
                    // profile's origin registry persisted in
                    // POLYSCALE_CEF_DATA_DIR.
                    let result_path = site_data_result_path
                        .clone()
                        .expect("site-data test has a result path");
                    let window = cx.window.clone();
                    append_result(&result_path, &format!("LOAD:{localhost_origin}/site-data"));
                    cx.tasks.spawn(async move {
                        let deadline =
                            std::time::Instant::now() + std::time::Duration::from_secs(20);
                        let served =
                            wait_for_result_line(&result_path, "HTTP:/site-data\n", deadline).await
                                && wait_for_result_line(
                                    &result_path,
                                    "HTTP:/site-data-frame",
                                    deadline,
                                )
                                .await;
                        if !served {
                            let _ = fs::write(&result_path, "FAIL:record-pages-not-served");
                            window.close();
                            return;
                        }
                        zgx::timer::sleep(std::time::Duration::from_millis(500)).await;
                        append_result(&result_path, "PHASE1:DONE");
                        window.close();
                    });
                    web.load_url(format!("{localhost_origin}/site-data"));
                } else if site_data_phase == "verify" {
                    // Restart scenario, process 2 (same data dir): the site is
                    // not visited this session, so only the persisted registry
                    // can name its origins; clearing from one of them must
                    // remove the other's data too.
                    let result_path = site_data_result_path
                        .clone()
                        .expect("site-data test has a result path");
                    let window = cx.window.clone();
                    let verify_web = web.clone();
                    let clear_origin = localhost_origin.clone();
                    let sibling = sibling_origin.clone();
                    let main_host = localhost_origin.clone();
                    cx.tasks.spawn(async move {
                        let deadline =
                            std::time::Instant::now() + std::time::Duration::from_secs(40);
                        // Load a page on another site (127.0.0.1), so no
                        // localhost origin is recorded this session.
                        if !wait_for_result_line(&result_path, "HTTP:/\n", deadline).await {
                            let _ = fs::write(&result_path, "FAIL:verify-page-not-served");
                            window.close();
                            return;
                        }
                        zgx::timer::sleep(std::time::Duration::from_millis(300)).await;
                        // The registry persisted by the record process names
                        // both same-site origins.
                        let registry_ok = std::env::var_os("POLYSCALE_CEF_DATA_DIR")
                            .map(std::path::PathBuf::from)
                            .and_then(|dir| {
                                let component = format!(
                                    "p-{}",
                                    "default"
                                        .bytes()
                                        .map(|byte| format!("{byte:02x}"))
                                        .collect::<String>()
                                );
                                let bytes =
                                    fs::read(dir.join(component).join("visited-origins.json"))
                                        .ok()?;
                                let origins: Vec<String> =
                                    serde_json::from_slice(&bytes).ok()?;
                                Some(origins.contains(&main_host) && origins.contains(&sibling))
                            })
                            .unwrap_or(false);
                        append_result(&result_path, &format!("REGISTRY:persisted={registry_ok}"));
                        if !registry_ok {
                            let _ = fs::write(&result_path, "FAIL:persisted-registry");
                            window.close();
                            return;
                        }
                        // Clear the site from a URL on one of its hosts.
                        let cleared_slot = Arc::new(Mutex::new(None::<Result<usize, String>>));
                        {
                            let slot = cleared_slot.clone();
                            verify_web.clear_site_data(&clear_origin, move |result| {
                                *slot.lock().unwrap_or_else(|poison| poison.into_inner()) =
                                    Some(result);
                            });
                        }
                        let cleared = loop {
                            if let Some(result) =
                                cleared_slot.lock().unwrap_or_else(|poison| poison.into_inner()).take()
                            {
                                break result;
                            }
                            if std::time::Instant::now() >= deadline {
                                break Err("clear_site_data timed out".to_owned());
                            }
                            zgx::timer::sleep(std::time::Duration::from_millis(50)).await;
                        };
                        append_result(&result_path, &format!("CLEAR:{cleared:?}"));
                        if let Err(error) = &cleared {
                            let _ = fs::write(&result_path, format!("FAIL:clear:{error}"));
                            window.close();
                            return;
                        }
                        // Cookies (HttpOnly and Domain=.localhost included)
                        // and localStorage of both same-site origins must be
                        // gone.
                        let expected = "cookie:fixture=absent;domain=absent|local:null";
                        verify_web.load_url(format!("{sibling}/verify"));
                        let sibling_title = wait_for_title(&verify_web, "cookie:", deadline).await;
                        append_result(&result_path, &format!("SIBLING:{sibling_title}"));
                        verify_web.load_url(format!("{main_host}/verify"));
                        let main_title = wait_for_title(&verify_web, "cookie:", deadline).await;
                        append_result(&result_path, &format!("MAIN:{main_title}"));
                        let success = sibling_title == expected && main_title == expected;
                        let _ = fs::write(
                            &result_path,
                            format!(
                                "{}:sibling={sibling_title}|main={main_title}",
                                if success { "PASS" } else { "FAIL" }
                            ),
                        );
                        window.close();
                    });
                    web.load_url(origin.clone());
                } else {
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
                }
            } else if policy_test {
                let page = web.clone();
                let result_path = policy_result_path
                    .clone()
                    .expect("policy test has a result path");
                let window = cx.window.clone();
                let policy_diagnostic = result_path.with_extension("diagnostic");
                web.set_navigation_policy(move |request| {
                    let _ = fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&policy_diagnostic)
                        .and_then(|mut file| {
                            writeln!(
                                file,
                                "POLICY url={} main={} kind={:?}",
                                request.url, request.main_frame, request.kind
                            )
                        });
                    !request.url.contains("/policy-denied")
                });
                web.load_url(format!("{origin}/policy"));
                cx.tasks.spawn(async move {
                    for _ in 0..200 {
                        if page.title() == "policy"
                            && !page.state().with_untracked(|state| state.loading)
                        {
                            break;
                        }
                        zgx::timer::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    if page.title() != "policy" {
                        let _ = fs::write(&result_path, "FAIL:policy-page-load");
                        window.close();
                        return;
                    }
                    // A denied link must be cancelled before any request.
                    page.evaluate_js("document.querySelector('#denied').click()", |_| {});
                    zgx::timer::sleep(std::time::Duration::from_secs(1)).await;
                    let url_after_denied = page.url();
                    // An allowed link must navigate.
                    page.evaluate_js("document.querySelector('#allowed').click()", |_| {});
                    let mut allowed_committed = false;
                    for _ in 0..100 {
                        if page.url().contains("/policy-allowed") {
                            allowed_committed = true;
                            break;
                        }
                        zgx::timer::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    let status = if url_after_denied.contains("/policy-denied") {
                        "FAIL:denied-navigation-committed"
                    } else if !allowed_committed {
                        "FAIL:allowed-navigation-blocked"
                    } else {
                        "PASS"
                    };
                    let _ = fs::write(&result_path, status);
                    window.close();
                });
            } else if popup_close_race_test {
                // Close the PARENT controller while a popup creation is still
                // pending, then assert the backend survives: no panic, no
                // double close, and the browser counts return to baseline.
                let result = popup_close_race_result_path
                    .clone()
                    .expect("popup close-race fixture has a result path");
                let diagnostic = result.with_extension("diagnostic");
                // The shared diagnostic fills up with pump traces and the
                // shared tracer stops early, so the race task keeps its own
                // log beside it.
                let race_log = result.with_extension("race");
                let race_note: Arc<dyn Fn(String)> = Arc::new(move |message| {
                    let _ = fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&race_log)
                        .and_then(|mut file| writeln!(file, "{message}"));
                });
                let window = cx.window.clone();
                let race_web = web.clone();
                let race_scene = cx.ui.scene.clone();
                let race_input = cx.ui.input.clone();
                web.load_url(format!("{origin}/popup-parent"));
                cx.tasks.spawn(async move {
                    let mut ready = false;
                    for _ in 0..200 {
                        if race_web.title() == "popup parent"
                            && !race_web.state().with_untracked(|state| state.loading)
                        {
                            ready = true;
                            break;
                        }
                        zgx::timer::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    if !ready {
                        let _ = fs::write(&result, "FAIL:popup-close-race-parent-load");
                        window.close();
                        return;
                    }
                    race_note(format!(
                        "RACE_READY title={:?} url={:?} new_windows={}",
                        race_web.title(),
                        race_web.url(),
                        race_web.options().new_windows
                    ));
                    // Click the popup button with real pointer input first: a
                    // synthetic JS click has no user gesture, so Chromium
                    // would block window.open.
                    let scene = race_scene.clone();
                    let input = race_input.clone();
                    let note = race_note.clone();
                    let click_web = race_web.clone();
                    race_web.evaluate_js(
                        "(()=>{const r=document.querySelector('#open-popup').getBoundingClientRect();return {x:r.x+r.width/2,y:r.y+r.height/2}})()",
                        move |bounds| {
                            let position = bounds
                                .ok()
                                .and_then(|value| serde_json::from_str::<serde_json::Value>(&value).ok())
                                .and_then(|value| {
                                    Some((value["x"].as_f64()? as f32, value["y"].as_f64()? as f32))
                                });
                            let Some((x, y)) = position else {
                                note("CLICK_BOUNDS_MISSING".to_owned());
                                return;
                            };
                            let _ = input.dispatch(&scene, InputEvent::PointerMove { x, y });
                            let _ = input.dispatch(&scene, InputEvent::PointerDown {
                                x,
                                y,
                                button: PointerButton::Primary,
                            });
                            let _ = input.dispatch(&scene, InputEvent::PointerUp {
                                x,
                                y,
                                button: PointerButton::Primary,
                            });
                            note(format!("POINTER_CLICK_DISPATCHED:{x}:{y}"));
                            // Fall back to a direct JS click if the pointer
                            // input missed (the page may not be laid out yet).
                            let clicked_web = click_web.clone();
                            clicked_web.evaluate_js("window.__popupClicked||0", move |clicked| {
                                let clicked = clicked
                                    .ok()
                                    .and_then(|value| value.parse::<u64>().ok())
                                    .unwrap_or(0);
                                note(format!("CLICKED_STATE:{clicked}"));
                                if clicked == 0 {
                                    click_web.evaluate_js(
                                        "document.querySelector('#open-popup').click(); 'clicked'",
                                        |_| {},
                                    );
                                }
                            });
                        },
                    );
                    // Wait until CEF reports the pending popup creation.
                    let mut pending = false;
                    for _ in 0..400 {
                        if fs::read_to_string(&diagnostic)
                            .is_ok_and(|log| log.contains("CEF_BEFORE_POPUP:"))
                        {
                            pending = true;
                            break;
                        }
                        zgx::timer::sleep(std::time::Duration::from_millis(25)).await;
                    }
                    race_note(format!("PENDING={pending}"));
                    if !pending {
                        let _ = fs::write(&result, "FAIL:popup-close-race-no-pending-popup");
                        window.close();
                        return;
                    }
                    trace_close_race(
                        Some(&result),
                        "FIXTURE_CLOSE_PARENT_WITH_PENDING_POPUP",
                    );
                    // The race: drop the parent controller mid-creation.
                    race_web.close();
                    // Browser counts must return to baseline: every browser CEF
                    // created (the parent and, if it still completes, the
                    // popup) must close exactly once, and the pending popup
                    // creation must reach a terminal state (aborted, or
                    // created then closed).
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                    loop {
                        let log = fs::read_to_string(&diagnostic).unwrap_or_default();
                        let created = log
                            .lines()
                            .filter(|line| line.starts_with("ON_AFTER_CREATED id="))
                            .count();
                        let closed = log
                            .lines()
                            .filter(|line| line.starts_with("ON_BEFORE_CLOSE browser="))
                            .count();
                        let popup_terminal = created >= 2
                            || log.lines().any(|line| line.starts_with("CREATE_REJECTED id="));
                        if created >= 1 && created == closed && popup_terminal {
                            trace_close_race(Some(&result), "FIXTURE_BROWSER_COUNTS_BASELINE");
                            break;
                        }
                        if std::time::Instant::now() >= deadline {
                            let _ = fs::write(
                                &result,
                                format!(
                                    "FAIL:popup-close-race-counts:created={created}:closed={closed}"
                                ),
                            );
                            window.close();
                            return;
                        }
                        zgx::timer::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    window.close();
                });
            } else if popup_test {
                let result = popup_result_path
                    .clone()
                    .expect("popup fixture has a result path");
                let expected_origin = origin.clone();
                let window = cx.window.clone();
                let done = Arc::new(AtomicBool::new(false));
                let callback_done = done.clone();
                let timeout_done = done.clone();
                let timeout_path = result.clone();
                let timeout_window = window.clone();
                let timeout_web = web.clone();
                let diagnostics = result.clone();
                let callback_result = result.clone();
                // The popup child has its own controller: none of its events
                // may reach the parent's listeners.
                let leaked = Arc::new(AtomicBool::new(false));
                let leaked_in_callback = leaked.clone();
                let subscription = web.on_event(move |event| {
                    match &event {
                        zgx::web_view::WebViewEvent::TitleChanged { title }
                            if title == "popup child" =>
                        {
                            leaked.store(true, Ordering::Release);
                        }
                        zgx::web_view::WebViewEvent::UrlChanged { url }
                        | zgx::web_view::WebViewEvent::NavigationStarted { url }
                        | zgx::web_view::WebViewEvent::NavigationCommitted { url }
                        | zgx::web_view::WebViewEvent::NavigationFinished { url }
                            if url.contains("/popup-child") =>
                        {
                            leaked.store(true, Ordering::Release);
                        }
                        _ => {}
                    }
                    if let zgx::web_view::WebViewEvent::NewWindowRequested { url } = &event {
                        let _ = fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(diagnostics.with_extension("diagnostic"))
                            .and_then(|mut file| writeln!(file, "POPUP_REQUEST:{url}"));
                        return;
                    }
                    let zgx::web_view::WebViewEvent::Message {
                        body,
                        origin,
                        main_frame: true,
                    } = event
                    else {
                        return;
                    };
                    let _ = fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(diagnostics.with_extension("diagnostic"))
                        .and_then(|mut file| writeln!(file, "POPUP_APP_MESSAGE:{origin}:{body}"));
                    if origin != &expected_origin {
                        return;
                    }
                    let Ok(message) = serde_json::from_str::<serde_json::Value>(body) else {
                        return;
                    };
                    if message["source"] != "popup-parent" {
                        return;
                    }
                    if !callback_done.swap(true, Ordering::AcqRel) {
                        let result = if leaked_in_callback.load(Ordering::Acquire) {
                            "FAIL:popup-events-leaked-into-parent".to_owned()
                        } else if message["ok"] == true {
                            "PASS".to_owned()
                        } else {
                            format!("FAIL:popup-message:{message}")
                        };
                        let _ = fs::write(&callback_result, result);
                        window.close();
                    }
                });
                cx.tasks.spawn(async move {
                    zgx::timer::sleep(std::time::Duration::from_secs(12)).await;
                    if !timeout_done.swap(true, Ordering::AcqRel) {
                        let _ = fs::write(
                            timeout_path,
                            format!("FAIL:popup-timeout:title={}", timeout_web.title()),
                        );
                        timeout_window.close();
                    }
                });
                popup_subscription = Some(subscription);
                web.load_url(format!("{origin}/popup-parent"));
                let popup_page = web.clone();
                let popup_scene = cx.ui.scene.clone();
                let popup_input = cx.ui.input.clone();
                let popup_result = result.clone();
                let popup_window = cx.window.clone();
                cx.tasks.spawn(async move {
                    let mut ready = false;
                    for _ in 0..200 {
                        if popup_page.title() == "popup parent"
                            && !popup_page.state().with_untracked(|state| state.loading)
                        {
                            ready = true;
                            break;
                        }
                        zgx::timer::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    if !ready {
                        let _ = fs::write(&popup_result, "FAIL:popup-parent-load");
                        popup_window.close();
                        return;
                    }
                    let position_result = popup_result.clone();
                    let scene = popup_scene.clone();
                    let input = popup_input.clone();
                    let window = popup_window.clone();
                    let popup_check_page = popup_page.clone();
                    popup_page.evaluate_js(
                        "(()=>{const r=document.querySelector('#open-popup').getBoundingClientRect();return {x:r.x+r.width/2,y:r.y+r.height/2}})()",
                        move |result| {
                            let position = result
                                .ok()
                                .and_then(|value| serde_json::from_str::<serde_json::Value>(&value).ok())
                                .and_then(|value| Some((value["x"].as_f64()? as f32, value["y"].as_f64()? as f32)));
                            let Some((x, y)) = position else {
                                let _ = fs::write(position_result, "FAIL:popup-button-bounds");
                                window.close();
                                return;
                            };
                            let moved = input.dispatch(&scene, InputEvent::PointerMove { x, y });
                            let down = input.dispatch(&scene, InputEvent::PointerDown {
                                x,
                                y,
                                button: PointerButton::Primary,
                            });
                            let up = input.dispatch(&scene, InputEvent::PointerUp {
                                x,
                                y,
                                button: PointerButton::Primary,
                            });
                            let _ = fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(position_result.with_extension("diagnostic"))
                                .and_then(|mut file| writeln!(
                                    file,
                                    "INPUT_TARGETS:{:?}:{:?}:{:?}",
                                    moved.target,
                                    down.target,
                                    up.target
                                ));
                            let click_result = position_result.clone();
                            let retry_page = popup_check_page.clone();
                            popup_check_page.evaluate_js("window.__popupClicked||0", move |clicked| {
                                let _ = fs::OpenOptions::new()
                                    .create(true)
                                    .append(true)
                                    .open(click_result.with_extension("diagnostic"))
                                    .and_then(|mut file| writeln!(file, "CLICK_COUNT:{clicked:?}"));
                                // Synthetic pointer input can miss if the page was
                                // not laid out yet; fall back to a direct JS click.
                                let clicked = clicked.ok().and_then(|value| value.parse::<u64>().ok()).unwrap_or(0);
                                if clicked == 0 {
                                    retry_page.evaluate_js(
                                        "document.querySelector('#open-popup').click(); 'clicked'",
                                        |_| {},
                                    );
                                }
                            });
                        },
                    );
                });
            } else if sandbox_origin_test {
                web.load_url(format!("{origin}/sandbox-parent"));
            } else if event_test {
                web.load_html(EVENT_FIXTURE, None);
                let probe_web = web.clone();
                let web = web.clone();
                cx.tasks.spawn(async move {
                    zgx::timer::sleep(std::time::Duration::from_millis(300)).await;
                    web.load_html(EVENT_CHANGED_FIXTURE, None);
                });
                // Diagnostic probe (CDP path, independent of the V8 bridge):
                // which bridge entry points exist in the main frame document?
                let probe_path = event_result_path.clone().expect("event test has result path");
                cx.tasks.spawn(async move {
                    zgx::timer::sleep(std::time::Duration::from_millis(2_500)).await;
                    probe_web.evaluate_js(
                        "JSON.stringify({native:typeof globalThis.__zgxNativePostMessage,zgx:typeof globalThis.zgx,zgxPost:typeof globalThis.zgx?.postMessage,webkit:typeof globalThis.webkit?.messageHandlers?.zgx?.postMessage})",
                        move |result| {
                            if let Ok(mut log) = fs::OpenOptions::new().append(true).open(&probe_path) {
                                let _ = writeln!(log, "PROBE:{result:?}");
                            }
                        },
                    );
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
            } else if html_base_test {
                let expected_origin = origin.clone();
                let result_path = html_base_result_path
                    .clone()
                    .expect("HTML base test has a result path");
                let window = cx.window.clone();
                let page = web.clone();
                let timeout_path = result_path.clone();
                let timeout_window = window.clone();
                web.load_html(
                    "<!doctype html><meta charset=utf-8><title>HTML base origin</title><p id=fixture>served by CEF</p>",
                    Some(&origin),
                );
                cx.tasks.spawn(async move {
                    let mut ready = false;
                    for _ in 0..200 {
                        if page.title() == "HTML base origin"
                            && !page.state().with_untracked(|state| state.loading)
                        {
                            ready = true;
                            break;
                        }
                        zgx::timer::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    if !ready {
                        let state = page.state();
                        let (url, loading, error) = state.with_untracked(|state| {
                            (state.url.clone(), state.loading, state.error.clone())
                        });
                        let _ = fs::write(
                            &timeout_path,
                            format!(
                                "FAIL:base-document-not-served:title={}:url={url}:loading={loading}:error={error:?}",
                                page.title(),
                            ),
                        );
                        timeout_window.close();
                        return;
                    }
                    let eval_result_path = result_path.clone();
                    let eval_window = window.clone();
                    page.evaluate_js("location.origin", move |result| {
                        let expected = serde_json::to_string(&expected_origin).unwrap();
                        let passed = result.as_deref() == Ok(expected.as_str());
                        let status = if passed {
                            "PASS".to_owned()
                        } else {
                            format!("FAIL:origin:{result:?}:expected:{expected}")
                        };
                        let _ = fs::write(eval_result_path, status);
                        eval_window.close();
                    });
                    zgx::timer::sleep(std::time::Duration::from_secs(10)).await;
                    if fs::read_to_string(&result_path).is_ok_and(|status| status == "START\n") {
                        let _ = fs::write(result_path, "TIMEOUT:origin-evaluation");
                        window.close();
                    }
                });
            } else if input_test {
                web.load_url(origin);
                let input_page = web.clone();
                let input_window = cx.window.clone();
                let input_tasks = cx.tasks.clone();
                let scene = cx.ui.scene.clone();
                let input = cx.ui.input.clone();
                let input_result = input_result_path
                    .clone()
                    .expect("input test has result path");
                cx.tasks.spawn(async move {
                    let mut ready = false;
                    for _ in 0..200 {
                        if input_page.title() == "CEF OSR fixture"
                            && !input_page.state().with_untracked(|state| state.loading)
                        {
                            ready = true;
                            break;
                        }
                        zgx::timer::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    if !ready {
                        let _ = fs::write(&input_result, "TIMEOUT:fixture-page");
                        input_window.close();
                        return;
                    }
                    trace_close_race(Some(&input_result), "INPUT_PAGE_READY");
                    let page = input_page.clone();
                    let scene = scene.clone();
                    input_page.evaluate_js(
                        "(()=>{const r=document.querySelector('#typing').getBoundingClientRect();return {x:r.x+r.width/2,y:r.y+r.height/2}})()",
                        move |result| {
                            trace_close_race(
                                Some(&input_result),
                                &format!("INPUT_BOUNDS_RESULT:{result:?}"),
                            );
                            let position = result
                                .ok()
                                .and_then(|value| serde_json::from_str::<serde_json::Value>(&value).ok())
                                .and_then(|value| Some((value["x"].as_f64()? as f32, value["y"].as_f64()? as f32)));
                            let Some((x, y)) = position else {
                                let _ = fs::write(&input_result, "FAIL:input-bounds");
                                input_window.close();
                                return;
                            };
                            if !skip_input {
                                input.dispatch(&scene, InputEvent::PointerMove { x, y });
                                input.dispatch(&scene, InputEvent::PointerDown { x, y, button: PointerButton::Primary });
                                input.dispatch(&scene, InputEvent::PointerUp { x, y, button: PointerButton::Primary });
                                input.dispatch(&scene, InputEvent::Focus);
                                trace_close_race(Some(&input_result), "INPUT_POINTER_AND_FOCUS_SENT");
                                input.dispatch(&scene, InputEvent::KeyDown { key: Key::Character("a".into()), modifiers: Modifiers::default(), repeat: false });
                                input.dispatch(&scene, InputEvent::Text("fixture-".into()));
                                input.dispatch(&scene, InputEvent::KeyUp { key: Key::Character("a".into()), modifiers: Modifiers::default() });
                                trace_close_race(Some(&input_result), "INPUT_KEY_EVENTS_SENT");
                            }
                            if !skip_input && !skip_ime {
                                input.dispatch(&scene, InputEvent::ImePreedit { text: "🦀".into(), cursor: Some((4, 4)) });
                                input.dispatch(&scene, InputEvent::ImeCommit("🦀".into()));
                                trace_close_race(Some(&input_result), "INPUT_IME_EVENTS_SENT");
                            }
                            input_tasks.spawn(async move {
                                // CEF forwards OSR IME updates asynchronously to
                                // the renderer; wait for that renderer task
                                // before issuing the readback over DevTools.
                                zgx::timer::sleep(std::time::Duration::from_millis(100)).await;
                                trace_close_race(Some(&input_result), "INPUT_READBACK_REQUESTED");
                                // Keep the WebView controller alive through
                                // the asynchronous DevTools result. The task
                                // only needs it to enqueue the request; without
                                // this capture it can drop the last fixture
                                // handle before CEF replies.
                                let keep_alive = page.clone();
                                page.evaluate_js(
                                    "document.querySelector('#typing').value",
                                    move |value| {
                                        drop(keep_alive);
                                        trace_close_race(
                                            Some(&input_result),
                                            &format!("INPUT_READBACK_RESULT:{value:?}"),
                                        );
                                        let expected = if skip_input { "\"\"" } else if skip_ime { "\"fixture-\"" } else { "\"fixture-🦀\"" };
                                        let passed = value.as_deref() == Ok(expected);
                                        let status = if passed {
                                            "PASS".to_owned()
                                        } else {
                                            format!("FAIL:{value:?}")
                                        };
                                        let _ = fs::write(&input_result, status);
                                        input_window.close();
                                    },
                                );
                            });
                        },
                    );
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
            } else if !close_race_test && !navigation_eval_race_test && !popup_close_race_test {
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
            let app = if close_race_test {
                let page = web.clone();
                let window = cx.window.clone();
                let result_path = close_race_result_path.clone();
                zgx::web_view::web_view(&web)
                    .grow()
                    .w_full()
                    .h_full()
                    .on_mount(move |_| {
                        trace_close_race(result_path.as_deref(), "FIXTURE_ON_MOUNT");
                        if !page.is_closed() {
                            trace_close_race(
                                result_path.as_deref(),
                                "FIXTURE_FAIL_CONTROLLER_STILL_OPEN",
                            );
                        }
                        trace_close_race(result_path.as_deref(), "FIXTURE_WINDOW_CLOSE");
                        window.close();
                    })
            } else if stress_test {
                let current = cx.ui.signal(0usize);
                let selected = current.clone();
                let pages_for_view = stress_pages.clone();
                let result_path = stress_result_path.clone().expect("stress result path");
                let diagnostic = result_path.with_extension("diagnostic");
                let pages_for_task = stress_pages.clone();
                let selected_for_task = current.clone();
                let window_for_task = cx.window.clone();
                cx.tasks.spawn(async move {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
                    for (index, page) in pages_for_task.iter().enumerate() {
                        selected_for_task.set(index);
                        let created = wait_for_diagnostic_count(
                            &diagnostic,
                            "ON_AFTER_CREATED ",
                            index + 1,
                            deadline,
                        )
                        .await;
                        if !created {
                            let _ = fs::write(
                                &result_path,
                                format!("FAIL:create-timeout:{index}:{}", index + 1),
                            );
                            for page in &pages_for_task {
                                page.close();
                            }
                            window_for_task.close();
                            return;
                        }

                        page.close();
                        let closed = wait_for_diagnostic_count(
                            &diagnostic,
                            "ON_BEFORE_CLOSE ",
                            index + 1,
                            deadline,
                        )
                        .await;
                        if !closed {
                            let _ = fs::write(
                                &result_path,
                                format!("FAIL:close-timeout:{index}:{}", index + 1),
                            );
                            for page in &pages_for_task {
                                page.close();
                            }
                            window_for_task.close();
                            return;
                        }
                    }
                    window_for_task.close();
                });
                switch(
                    move || selected.get(),
                    move |index, _| {
                        zgx::web_view::web_view(&pages_for_view[index])
                            .grow()
                            .w_full()
                            .h_full()
                    },
                )
                .grow()
                .w_full()
                .h_full()
            } else if late_load_test {
                row()
                    .w_full()
                    .h_full()
                    .child(zgx::web_view::web_view(&web).grow())
                    .child(zgx::web_view::web_view(late_shared.as_ref().unwrap()).grow())
                    .child(zgx::web_view::web_view(late_isolated.as_ref().unwrap()).grow())
            } else if drag_test {
                let source_rect_capture = source_rect.clone();
                let target_rect_capture = target_rect.clone();
                let source_view = zgx::web_view::web_view(&web)
                    .w(290.)
                    .h_full()
                    .on_mount(move |handle| {
                        capture_fixture_rect(handle, source_rect_capture.clone())
                    });
                let target_view = zgx::web_view::web_view(
                    drag_target_web.as_ref().expect("drag target WebView exists"),
                )
                .w(290.)
                .h_full()
                .on_mount(move |handle| {
                    capture_fixture_rect(handle, target_rect_capture.clone())
                });
                row()
                    .w_full()
                    .h_full()
                    .child(source_view)
                    .child(drag_file_source.grow())
                    .child(target_view)
            } else if download_test || input_test || popup_test || popup_close_race_test || html_base_test {
                zgx::web_view::web_view(&web)
                    .grow()
                    .w_full()
                    .h_full()
            } else {
                app
            };
            let app = if event_subscription.is_some()
                || sandbox_subscription.is_some()
                || popup_subscription.is_some()
                || drag_subscription.is_some()
            {
                app.on_mount(move |handle| {
                    if let Some(subscription) = event_subscription {
                        handle.retain(subscription);
                    }
                    if let Some(subscription) = sandbox_subscription {
                        handle.retain(subscription);
                    }
                    if let Some(subscription) = popup_subscription {
                        handle.retain(subscription);
                    }
                    if let Some(subscription) = drag_subscription {
                        handle.retain(subscription);
                    }
                })
            } else {
                app
            };
            let _view = cx.render(app);
        });
    if let Some(result_path) = input_result_path.as_ref() {
        trace_close_race(
            Some(result_path),
            if run_result.is_ok() {
                "INPUT_APPLICATION_RETURNED_OK"
            } else {
                "INPUT_APPLICATION_RETURNED_ERROR"
            },
        );
        if let Err(error) = &run_result {
            fs::write(result_path, format!("FAIL:application:{error}"))?;
        }
    }
    if let Some(result_path) = close_race_result_path {
        trace_close_race(
            Some(&result_path),
            if run_result.is_ok() {
                "APPLICATION_RETURNED_OK"
            } else {
                "APPLICATION_RETURNED_ERROR"
            },
        );
        let diagnostic = fs::read_to_string(result_path.with_extension("diagnostic"))?;
        let positions = [
            "CEF_CREATE_ACCEPTED",
            "FIXTURE_PAGE_CLOSE_AFTER_ACCEPTED",
            "CEF_ON_AFTER_CREATED",
            "ON_AFTER_CREATED id=1 browser=1 active=false",
            "CLOSE_REQUEST_DEFERRED reason=late_create",
            "CEF_PUMP_RETURN",
            "CLOSE_REQUEST_DISPATCHED",
            "ON_BEFORE_CLOSE",
        ]
        .map(|marker| diagnostic.find(marker));
        let deferred_close_ok = positions.iter().all(Option::is_some)
            && positions.windows(2).all(|pair| pair[0] < pair[1]);
        let pass = run_result.is_ok() && deferred_close_ok;
        fs::write(result_path, if pass { "PASS" } else { "FAIL" })?;
        if !deferred_close_ok {
            return Err("close-race fixture missed a deferred CEF close lifecycle event".into());
        }
    }
    if let Some(result_path) = stress_result_path {
        let diagnostic = fs::read_to_string(result_path.with_extension("diagnostic"))?;
        let created_ids: std::collections::HashSet<_> = diagnostic
            .lines()
            .filter_map(|line| line.strip_prefix("ON_AFTER_CREATED id="))
            .filter_map(|line| line.split_whitespace().next())
            .collect();
        let closed_ids: std::collections::HashSet<_> = diagnostic
            .lines()
            .filter_map(|line| line.strip_prefix("ON_BEFORE_CLOSE browser="))
            .filter_map(|line| line.split_whitespace().next())
            .collect();
        let counts_are_exact = created_ids.len() == CEF_STRESS_CYCLES
            && closed_ids.len() == CEF_STRESS_CYCLES
            && created_ids == closed_ids;
        let shutdown_is_empty = diagnostic
            .lines()
            .any(|line| line == "SHUTDOWN_BEGIN pending=0 open=0");
        let pass = run_result.is_ok() && counts_are_exact && shutdown_is_empty;
        fs::write(
            &result_path,
            format!(
                "{}:created={}:closed={}:shutdown_empty={}",
                if pass { "PASS" } else { "FAIL" },
                created_ids.len(),
                closed_ids.len(),
                shutdown_is_empty,
            ),
        )?;
        if !pass {
            return Err("CEF stress fixture did not close all created browsers cleanly".into());
        }
    }
    if let Some(result_path) = popup_close_race_result_path {
        trace_close_race(
            Some(&result_path),
            if run_result.is_ok() {
                "APPLICATION_RETURNED_OK"
            } else {
                "APPLICATION_RETURNED_ERROR"
            },
        );
        let already_failed =
            fs::read_to_string(&result_path).is_ok_and(|content| content.starts_with("FAIL"));
        if already_failed {
            return Err("popup close-race fixture reported a failure during the run".into());
        }
        // Reuse the close-race assertions: every browser CEF created closed
        // exactly once, and shutdown found no pending creation or open
        // browser (the runtime's own browser_created/browser_closed asserts
        // make a double close or an unknown close panic, which fails the run).
        let diagnostic = fs::read_to_string(result_path.with_extension("diagnostic"))?;
        let created_browsers: std::collections::HashSet<String> = diagnostic
            .lines()
            .filter_map(|line| line.strip_prefix("ON_AFTER_CREATED id="))
            .filter_map(|rest| {
                rest.split_whitespace()
                    .find_map(|word| word.strip_prefix("browser="))
                    .map(str::to_owned)
            })
            .collect();
        let closed_browsers: std::collections::HashSet<String> = diagnostic
            .lines()
            .filter_map(|line| line.strip_prefix("ON_BEFORE_CLOSE browser="))
            .filter_map(|rest| rest.split_whitespace().next().map(str::to_owned))
            .collect();
        let counts_baseline = !created_browsers.is_empty() && created_browsers == closed_browsers;
        let shutdown_empty = diagnostic
            .lines()
            .any(|line| line == "SHUTDOWN_BEGIN pending=0 open=0");
        let popup_was_exercised = diagnostic
            .lines()
            .any(|line| line.starts_with("CEF_BEFORE_POPUP:"));
        let pass = run_result.is_ok() && counts_baseline && shutdown_empty && popup_was_exercised;
        fs::write(
            &result_path,
            format!(
                "{}:created={}:closed={}:shutdown_empty={}:popup_seen={}",
                if pass { "PASS" } else { "FAIL" },
                created_browsers.len(),
                closed_browsers.len(),
                shutdown_empty,
                popup_was_exercised,
            ),
        )?;
        if !pass {
            return Err(
                "popup close-race fixture did not return browser counts to baseline".into(),
            );
        }
    }
    if external_data_dir.is_none() {
        let _ = fs::remove_dir_all(fixture_data_dir);
    }
    run_result?;
    Ok(())
}

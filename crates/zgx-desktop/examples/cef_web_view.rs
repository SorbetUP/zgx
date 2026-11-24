// Local CEF OSR smoke page. Run from a CEF bundle so Chromium's framework,
// resources and sandbox helper are beside the app executable:
// `cargo run -p zgx-desktop --features cef --bin cef_web_view`.
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    sync::{Arc, Mutex},
    thread,
};
use zgx::{
    compose::prelude::*,
    web_view::{WebView, WebViewOptions},
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
"#;

fn start_fixture_server(download_test: bool) -> Result<String, Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let address = listener.local_addr()?;
    let origin = format!("http://{address}");
    let result_path = std::env::var_os("ZGX_CEF_DOWNLOAD_RESULT").map(std::path::PathBuf::from);
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let result_path = result_path.clone();
            thread::spawn(move || {
                let mut stream = stream;
                let mut request = [0; 2048];
                let count = stream.read(&mut request).unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..count]);
                let path = request.split_whitespace().nth(1).unwrap_or("/");
                if let Some(result_path) = result_path.as_ref() {
                    if let Ok(mut result) = fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(result_path)
                    {
                        let _ = writeln!(result, "HTTP:{path} download={download_test}");
                    }
                }
                let (status, content_type, headers, body) =
                    if download_test && (path == "/download" || path == "/") {
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
    let download_test = std::env::var_os("ZGX_CEF_DOWNLOAD_RESULT").is_some();
    let event_result_path = std::env::var_os("ZGX_CEF_EVENT_RESULT").map(std::path::PathBuf::from);
    let event_test = event_result_path.is_some();
    let automated = download_test || event_test;
    if let Some(result_path) = std::env::var_os("ZGX_CEF_DOWNLOAD_RESULT") {
        fs::write(result_path, format!("START:auto={automated}\n"))?;
    }
    if let Some(result_path) = event_result_path.as_ref() {
        fs::write(result_path, "START\n")?;
    }
    let origin = start_fixture_server(download_test)?;
    if let Some(result_path) = event_result_path.as_ref() {
        let mut result = fs::OpenOptions::new().append(true).open(result_path)?;
        writeln!(result, "ORIGIN:{origin}")?;
    }
    Application::new()
        .window(WindowOptions {
            title: "CEF OSR fixture".into(),
            width: 900.,
            height: 650.,
            visible: true,
            bounds: automated.then(|| WindowBounds::new(900, 650).at(-10_000, -10_000)),
            ..Default::default()
        })
        .run(|cx| {
            let web = WebView::new(&cx.ui.runtime, WebViewOptions::default());
            let download_path =
                std::env::temp_dir().join(format!("zgx-cef-download-{}.txt", std::process::id()));
            let _ = fs::remove_file(&download_path);
            let completed_path = download_path.clone();
            let window = cx.window.clone();
            let result_path =
                std::env::var_os("ZGX_CEF_DOWNLOAD_RESULT").map(std::path::PathBuf::from);
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
                let observed = Arc::new(Mutex::new((false, false)));
                let event_state = observed.clone();
                let log_path = result_path.clone();
                let window = cx.window.clone();
                web.on_event(move |event| {
                    let label = match event {
                        zgx::web_view::WebViewEvent::TitleChanged { title }
                            if title == "CEF event changed" =>
                        {
                            "TITLE"
                        }
                        zgx::web_view::WebViewEvent::UrlChanged { url }
                            if url.contains("cef-event-url-changed") =>
                        {
                            "URL"
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
                        zgx::web_view::WebViewEvent::UrlChanged { url }
                            if url.contains("cef-event-url-changed") =>
                        {
                            observed.1 = true
                        }
                        _ => {}
                    }
                    if observed.0 && observed.1 {
                        let _ = fs::write(&result_path, "PASS");
                        window.close();
                    }
                })
            });
            if event_test {
                web.load_html(EVENT_FIXTURE, None);
                let web = web.clone();
                cx.tasks.spawn(async move {
                    zgx::timer::sleep(std::time::Duration::from_millis(300)).await;
                    web.load_html(EVENT_CHANGED_FIXTURE, None);
                });
            } else if download_test {
                web.load_url(origin);
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
            let app = if let Some(subscription) = event_subscription {
                app.on_mount(move |handle| handle.retain(subscription))
            } else {
                app
            };
            let _view = cx.render(app);
        })
}

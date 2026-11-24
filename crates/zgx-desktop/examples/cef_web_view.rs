// Local CEF OSR smoke page. Run from a CEF bundle so Chromium's framework,
// resources and sandbox helper are beside the app executable:
// `cargo run -p zgx-desktop --features cef --bin cef_web_view`.
use zgx::{
    compose::prelude::*,
    web_view::{WebView, WebViewOptions},
};
use zgx_desktop::{Application, WindowOptions};
use std::fs;

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
<a id="download" download="cef-fixture.txt" href="data:text/plain,cef-download-payload">Download fixture</a></div></main>
"#;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    Application::new()
        .window(WindowOptions {
            title: "CEF OSR fixture".into(),
            width: 900.,
            height: 650.,
            ..Default::default()
        })
        .run(|cx| {
            let web = WebView::new(&cx.ui.runtime, WebViewOptions::default());
            let download_path = std::env::temp_dir()
                .join(format!("zgx-cef-download-{}.txt", std::process::id()));
            let _ = fs::remove_file(&download_path);
            let completed_path = download_path.clone();
            web.set_download_handler(move |request, responder| {
                println!("CEF_DOWNLOAD_REQUEST {} {}", request.name, request.mime);
                let completed_path = completed_path.clone();
                responder.save_to(download_path.clone(), move |status| match status {
                    zgx::web_view::DownloadStatus::Progress { received, total } => {
                        println!("CEF_DOWNLOAD_PROGRESS {received} {total:?}");
                    }
                    zgx::web_view::DownloadStatus::Finished => {
                        let matches = fs::read(&completed_path)
                            .is_ok_and(|bytes| bytes == b"cef-download-payload");
                        println!("CEF_DOWNLOAD_{}", if matches { "PASS" } else { "FAIL" });
                        let _ = fs::remove_file(&completed_path);
                    }
                    zgx::web_view::DownloadStatus::Failed(reason) => {
                        eprintln!("CEF_DOWNLOAD_FAILED {reason}");
                    }
                    _ => eprintln!("CEF_DOWNLOAD_UNKNOWN_STATUS"),
                });
            });
            web.load_html(FIXTURE, Some("https://fixture.invalid/"));
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
            let _view = cx.render(app);
        })
}

// Native desktop shell for the UltraGraph vis layer. This binary carries
// no logic of its own — it's a plain window pointed at whatever `ug serve`
// URL it's told about via UG_APP_URL. All actual UI/API work happens on
// the server it points at; `ug app` (see main.rs) is what launches that
// server and sets this env var before spawning this process.
fn main() {
    let url = std::env::var("UG_APP_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());
    let url: url::Url = url
        .parse()
        .unwrap_or_else(|e| panic!("UG_APP_URL {url:?} is not a valid URL: {e}"));

    tauri::Builder::default()
        .setup(move |app| {
            tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::External(url.clone()))
                .title("UltraGraph")
                .inner_size(1400.0, 900.0)
                .build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running ug-app");
}

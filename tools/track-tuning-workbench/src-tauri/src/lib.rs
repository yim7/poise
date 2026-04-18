pub mod config_document;
pub mod config_projection;

pub fn run() -> tauri::Result<()> {
    tauri::Builder::default().run(tauri::generate_context!())
}

pub mod binance_quote;
pub mod commands;
pub mod config_document;
pub mod config_projection;
pub mod error;
pub mod session_store;

pub fn run() -> tauri::Result<()> {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .invoke_handler(tauri::generate_handler![
            commands::load_config_file,
            commands::load_saved_draft,
            commands::save_draft,
            commands::risk_acquisition_defaults,
            commands::copy_text,
            commands::fetch_binance_quote,
            commands::export_current_track,
            commands::export_all_tracks
        ])
        .run(tauri::generate_context!())
}

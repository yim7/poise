pub fn run() -> tauri::Result<()> {
  tauri::Builder::default().run(tauri::generate_context!())
}

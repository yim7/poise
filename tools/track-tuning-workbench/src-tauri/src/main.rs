fn main() {
  if let Err(error) = poise_track_tuning_workbench::run() {
    eprintln!("failed to start Track Tuning Workbench: {error}");
    std::process::exit(1);
  }
}

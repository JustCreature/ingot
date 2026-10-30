//! The desktop app: library grid, loupe, culling, develop and import.

mod app;
mod images;
mod imports;
mod library;

use std::path::PathBuf;

pub use app::IngotApp;

/// Where the app keeps its own files.
#[derive(Clone, Debug)]
pub struct AppPaths {
    pub triage: PathBuf,
    pub history: PathBuf,
    pub imports: PathBuf,
    pub gdrive: PathBuf,
}

pub fn run(
    paths: AppPaths,
    folder: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([900.0, 560.0])
            .with_title("Ingot"),
        ..Default::default()
    };
    eframe::run_native(
        "Ingot",
        options,
        Box::new(move |creation| Ok(Box::new(IngotApp::new(&creation.egui_ctx, paths, folder)))),
    )
    .map_err(|err| format!("could not open the app window: {err}").into())
}

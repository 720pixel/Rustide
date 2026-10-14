mod app;
mod logo;
pub mod model;
#[cfg(target_os = "windows")]
mod native_dialog;
mod paint;
mod picker;
mod platform;
mod settings;

pub fn run() -> anyhow::Result<()> {
    let mut app = app::App::default();
    platform::run("Rustide", move |ctx| app.ui(ctx))
}

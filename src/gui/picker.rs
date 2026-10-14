use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PickMode {
    File,
    Folder,
    Save,
}

#[cfg(target_os = "windows")]
pub struct Picker {
    mode: PickMode,
    current: String,
    torrent_only: bool,
}

#[cfg(target_os = "windows")]
impl Picker {
    pub fn new(mode: PickMode, current: &str, torrent_only: bool) -> Self {
        Self {
            mode,
            current: current.into(),
            torrent_only,
        }
    }

    pub fn ui(&mut self, ctx: &egui::Context) -> anyhow::Result<(bool, Option<PathBuf>)> {
        super::native_dialog::pick(ctx, self.mode, &self.current, self.torrent_only)
            .map(|selected| (false, selected))
    }
}

#[cfg(not(target_os = "windows"))]
pub use fallback::Picker;

#[cfg(not(target_os = "windows"))]
mod fallback {
    use super::{PathBuf, PickMode};
    use egui::{RichText, ScrollArea, TextEdit};
    use std::fs;

    pub struct Picker {
        pub mode: PickMode,
        directory: PathBuf,
        location: String,
        filename: String,
        entries: Vec<(String, bool)>,
        error: String,
        filter: String,
        torrent_only: bool,
    }

    impl Picker {
        pub fn new(mode: PickMode, current: &str, torrent_only: bool) -> Self {
            let home = std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            let path = crate::gui::model::expand_path(current).unwrap_or(home.clone());
            let directory = if path.is_dir() {
                path.clone()
            } else {
                path.parent()
                    .filter(|p| p.is_dir())
                    .map(PathBuf::from)
                    .unwrap_or(home)
            };
            let mut picker = Self {
                mode,
                location: directory.to_string_lossy().into_owned(),
                directory,
                filename: if mode == PickMode::Save {
                    path.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or("untitled.torrent".into())
                } else {
                    String::new()
                },
                entries: Vec::new(),
                error: String::new(),
                filter: String::new(),
                torrent_only,
            };
            picker.refresh();
            picker
        }

        fn refresh(&mut self) {
            self.entries.clear();
            self.error.clear();
            let result = (|| -> std::io::Result<()> {
                for entry in fs::read_dir(&self.directory)?.take(10_001) {
                    let entry = entry?;
                    let is_dir = entry.path().is_dir();
                    if is_dir
                        || (self.mode != PickMode::Folder
                            && (!self.torrent_only
                                || entry
                                    .path()
                                    .extension()
                                    .is_some_and(|s| s.eq_ignore_ascii_case("torrent"))))
                    {
                        self.entries
                            .push((entry.file_name().to_string_lossy().into_owned(), is_dir));
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                self.error = error.to_string();
            }
            if self.entries.len() > 10_000 {
                self.entries.truncate(10_000);
                self.error =
                    "Showing the first 10,000 entries. Enter a path to select another file.".into();
            }
            self.entries.sort_by(|a, b| {
                b.1.cmp(&a.1)
                    .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
            });
            self.location = self.directory.to_string_lossy().into_owned();
        }

        pub fn ui(&mut self, ctx: &egui::Context) -> anyhow::Result<(bool, Option<PathBuf>)> {
            let mut open = true;
            let mut selected = None;
            let title = match self.mode {
                PickMode::File => "Choose a file",
                PickMode::Folder => "Choose a folder",
                PickMode::Save => "Save torrent as",
            };
            egui::Modal::new(egui::Id::new("file-picker")).show(ctx, |ui| {
                ui.set_width(610.0);
                ui.label(RichText::new(title).size(20.0).strong());
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("Home").clicked()
                        && let Some(home) =
                            std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
                    {
                        self.directory = home.into();
                        self.refresh();
                    }
                    if ui.button("Up").clicked()
                        && let Some(parent) = self.directory.parent()
                    {
                        self.directory = parent.into();
                        self.refresh();
                    }
                    let response =
                        ui.add(TextEdit::singleline(&mut self.location).desired_width(410.0));
                    if ui.button("Go").clicked()
                        || (response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                    {
                        match crate::gui::model::expand_path(&self.location) {
                            Ok(path) if path.is_dir() => {
                                self.directory = path;
                                self.refresh();
                            }
                            Ok(path) if path.is_file() && self.mode == PickMode::File => {
                                selected = Some(path);
                            }
                            _ => self.error = "That folder could not be opened.".into(),
                        }
                    }
                });
                ui.add_space(8.0);
                ui.add(
                    TextEdit::singleline(&mut self.filter)
                        .hint_text("Filter by name")
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(6.0);
                let mut navigate = None;
                ScrollArea::vertical()
                    .id_salt("file-picker-list")
                    .min_scrolled_height(290.0)
                    .max_height(350.0)
                    .show(ui, |ui| {
                        let filter = self.filter.to_lowercase();
                        for (name, directory) in &self.entries {
                            if !name.to_lowercase().contains(&filter) {
                                continue;
                            }
                            let text = if *directory {
                                format!("[folder]  {name}")
                            } else {
                                format!("           {name}")
                            };
                            let response = ui.selectable_label(self.filename == *name, text);
                            if response.clicked() {
                                if *directory {
                                    navigate = Some(self.directory.join(name));
                                } else {
                                    self.filename.clone_from(name);
                                }
                            }
                            if response.double_clicked() && !directory {
                                selected = Some(self.directory.join(name));
                            }
                        }
                    });
                if let Some(path) = navigate {
                    self.directory = path;
                    self.filter.clear();
                    self.refresh();
                }
                ui.add_space(8.0);
                if self.mode != PickMode::Folder {
                    ui.horizontal(|ui| {
                        ui.label("File name");
                        ui.add(
                            TextEdit::singleline(&mut self.filename).desired_width(f32::INFINITY),
                        );
                    });
                }
                if !self.error.is_empty() {
                    ui.label(RichText::new(&self.error).color(crate::gui::app::ERROR));
                }
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    let label = match self.mode {
                        PickMode::Folder => "Select this folder",
                        PickMode::Save => "Use this path",
                        PickMode::File => "Open file",
                    };
                    if ui.button(RichText::new(label).strong()).clicked() {
                        let path = if self.mode == PickMode::Folder {
                            self.directory.clone()
                        } else {
                            self.directory.join(self.filename.trim())
                        };
                        if self.mode == PickMode::Folder
                            || self.mode == PickMode::Save && !self.filename.trim().is_empty()
                            || path.is_file()
                        {
                            selected = Some(path);
                        } else {
                            self.error = "Choose a file first.".into();
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        selected = None;
                        self.error = "__cancel__".into();
                    }
                });
            });
            if self.error == "__cancel__" || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                open = false;
                selected = None;
            }
            if selected.is_some() {
                open = false;
            }
            Ok((open, selected))
        }
    }
}

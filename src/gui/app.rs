use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use egui::{Color32, Frame, RichText, ScrollArea, Stroke, TextEdit, Vec2};

use super::{
    model::{self, CreateForm, EditorForm, TreeRow},
    picker::{PickMode, Picker},
    settings::{LocationKind, Preferences},
};
use crate::{
    create::{self, CreateOptions, CreateProgress, CreateReport, IoMode, Protocol},
    metadata::{self, Document},
};

pub(super) const ERROR: Color32 = Color32::from_rgb(177, 55, 73);
const BG: Color32 = Color32::from_rgb(247, 242, 233);
const PANEL: Color32 = Color32::from_rgb(255, 251, 245);
const BORDER: Color32 = Color32::from_rgb(231, 220, 210);
const TEXT: Color32 = Color32::from_rgb(67, 35, 44);
const MUTED: Color32 = Color32::from_rgb(125, 100, 100);
const ACCENT: Color32 = Color32::from_rgb(114, 44, 63);
const PUBLIC_TRACKER: &str = "udp://tracker.opentrackr.org:1337/announce";

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Tab {
    #[default]
    Create,
    Edit,
}
#[derive(Clone, Copy)]
enum PickTarget {
    Input,
    Output,
    Open,
    SaveAs,
}

struct Editor {
    path: PathBuf,
    document: Document,
    form: EditorForm,
    rows: Vec<TreeRow>,
    dirty: bool,
    tree: bool,
    filter: String,
    selected: String,
    value: String,
    add_key: String,
    add_value: String,
    unlock_hashes: bool,
}

impl Editor {
    fn new(path: PathBuf, document: Document) -> Self {
        Self {
            path,
            form: EditorForm::from_document(&document),
            rows: model::tree_rows(&document.value),
            document,
            dirty: false,
            tree: false,
            filter: String::new(),
            selected: String::new(),
            value: String::new(),
            add_key: String::new(),
            add_value: "\"\"".into(),
            unlock_hashes: false,
        }
    }
    fn changed(&self) -> bool {
        self.dirty || self.form != EditorForm::from_document(&self.document)
    }
    fn apply_form(&mut self) -> Result<()> {
        let changed = self.form != EditorForm::from_document(&self.document);
        if changed {
            self.document = self.form.apply(&self.document)?;
            self.rows = model::tree_rows(&self.document.value);
            self.dirty = true;
        }
        Ok(())
    }
    fn replace_document(&mut self, document: Document) {
        self.document = document;
        self.form = EditorForm::from_document(&self.document);
        self.rows = model::tree_rows(&self.document.value);
        self.dirty = true;
        self.value.clear();
    }
}

struct Plan {
    protocol: Protocol,
    total: u64,
    pieces: usize,
    workers: usize,
    io: &'static str,
    hash: &'static str,
}
struct Status {
    phase: &'static str,
    plan: Option<Plan>,
    since: Instant,
}
struct WorkProgress {
    status: Mutex<Status>,
    bytes: AtomicU64,
}
impl WorkProgress {
    fn new(phase: &'static str) -> Self {
        Self {
            status: Mutex::new(Status {
                phase,
                plan: None,
                since: Instant::now(),
            }),
            bytes: AtomicU64::new(0),
        }
    }
    fn event(&self, event: CreateProgress) {
        if let CreateProgress::Hashed { bytes } = event {
            self.bytes.fetch_add(bytes, Ordering::Relaxed);
            return;
        }
        let mut state = self.status.lock().unwrap();
        state.phase = match event {
            CreateProgress::Scanning => "Scanning files",
            CreateProgress::Hashing {
                protocol,
                payload_bytes,
                piece_bytes,
                workers,
                io_backend,
                hash_backend,
                ..
            } => {
                self.bytes.store(0, Ordering::Relaxed);
                state.plan = Some(Plan {
                    protocol,
                    total: payload_bytes,
                    pieces: piece_bytes,
                    workers,
                    io: io_backend,
                    hash: hash_backend,
                });
                state.since = Instant::now();
                "Hashing payload"
            }
            CreateProgress::Checksums => "Calculating checksums",
            CreateProgress::Metadata => "Preparing metadata",
            CreateProgress::Hashed { .. } => unreachable!(),
        };
    }
    fn phase(&self, phase: &'static str) {
        self.status.lock().unwrap().phase = phase;
    }
}
enum Outcome {
    Created(PathBuf, CreateReport, Vec<String>),
    Opened(PathBuf, Document),
    Saved(PathBuf, Document),
}
struct Job {
    receiver: mpsc::Receiver<Result<Outcome>>,
    progress: Arc<WorkProgress>,
}
enum Confirm {
    Create(Box<CreateOptions>),
    Save(PathBuf, Document),
    DiscardOpen(PathBuf),
    Close,
    Remove(String),
}

pub(super) struct App {
    styled: bool,
    tab: Tab,
    create: CreateForm,
    editor: Option<Editor>,
    picker: Option<(PickTarget, Picker)>,
    job: Option<Job>,
    confirm: Option<Confirm>,
    notice: Option<(bool, String)>,
    created: Option<(PathBuf, CreateReport)>,
    preferences: Preferences,
}

impl Default for App {
    fn default() -> Self {
        let (preferences, notice) = match Preferences::load() {
            Ok(preferences) => (preferences, None),
            Err(error) => (Preferences::default(), Some((true, format!("{error:#}")))),
        };
        Self {
            styled: false,
            tab: Tab::Create,
            create: CreateForm::default(),
            editor: None,
            picker: None,
            job: None,
            confirm: None,
            notice,
            created: None,
            preferences,
        }
    }
}

impl App {
    pub(super) fn ui(&mut self, ctx: &egui::Context) {
        if !self.styled {
            style(ctx);
            self.styled = true;
        }
        self.poll();
        if ctx.input(|i| i.viewport().close_requested())
            && (self.job.is_some() || self.editor.as_ref().is_some_and(Editor::changed))
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm = Some(Confirm::Close);
        }
        if self.job.is_some() {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
        egui::SidePanel::left("navigation")
            .exact_width(184.0)
            .resizable(false)
            .frame(
                Frame::new()
                    .fill(Color32::from_rgb(239, 231, 221))
                    .inner_margin(20),
            )
            .show(ctx, |ui| {
                ui.add_space(9.0);
                ui.horizontal(|ui| {
                    let (rect, _) =
                        ui.allocate_exact_size(Vec2::new(27.0, 30.0), egui::Sense::hover());
                    super::logo::paint(ui.painter(), rect);
                    ui.label(RichText::new("Rustide").size(23.0).strong());
                });
                ui.add_space(7.0);
                ui.label(RichText::new("Torrent creator").size(11.0).color(MUTED));
                ui.add_space(37.0);
                for (tab, label) in [(Tab::Create, "Create torrent"), (Tab::Edit, "Edit torrent")] {
                    let selected = self.tab == tab;
                    let button = egui::Button::new(RichText::new(label).color(if selected {
                        ACCENT
                    } else {
                        TEXT
                    }))
                    .fill(if selected {
                        Color32::from_rgb(234, 216, 215)
                    } else {
                        Color32::TRANSPARENT
                    })
                    .min_size(Vec2::new(144.0, 42.0))
                    .stroke(Stroke::NONE)
                    .corner_radius(10);
                    if ui.add(button).clicked() {
                        self.tab = tab;
                    }
                    ui.add_space(19.0);
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.label(
                        RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                            .size(11.0)
                            .color(MUTED),
                    );
                });
            });
        egui::TopBottomPanel::bottom("actions")
            .resizable(false)
            .frame(
                Frame::new()
                    .fill(PANEL)
                    .inner_margin(egui::Margin::symmetric(26, 16)),
            )
            .show(ctx, |ui| {
                self.action_bar(ui);
            });
        egui::CentralPanel::default()
            .frame(Frame::new().fill(BG).inner_margin(26))
            .show(ctx, |ui| {
                if let Some((error, message)) = &self.notice {
                    let mut dismiss = false;
                    Frame::new()
                        .fill(if *error {
                            Color32::from_rgb(249, 226, 224)
                        } else {
                            Color32::from_rgb(239, 223, 220)
                        })
                        .corner_radius(10)
                        .inner_margin(12)
                        .show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(RichText::new(message).color(if *error {
                                    ERROR
                                } else {
                                    ACCENT
                                }));
                                dismiss = ui.small_button("Dismiss").clicked();
                            });
                        });
                    if dismiss {
                        self.notice = None;
                    }
                    ui.add_space(14.0);
                }
                ScrollArea::vertical()
                    .id_salt("main-content")
                    .show(ui, |ui| match self.tab {
                        Tab::Create => self.create_ui(ui),
                        Tab::Edit => self.edit_ui(ui),
                    });
            });
        self.picker_ui(ctx);
        self.confirm_ui(ctx);
    }

    fn create_ui(&mut self, ui: &mut egui::Ui) {
        hero(
            ui,
            "Create a torrent",
            "Choose your payload, set the details, and create.",
        );
        ui.add_enabled_ui(self.job.is_none(), |ui| {
            card(ui, |ui| {
                section(ui, "Payload & destination");
                label(ui, "File or folder");
                ui.horizontal(|ui| {
                    let response = ui.add(
                        TextEdit::singleline(&mut self.create.input)
                            .hint_text("Choose a file or folder…")
                            .desired_width((ui.available_width() - 180.0).max(100.0)),
                    );
                    if response.lost_focus() {
                        self.suggest_output();
                        self.remember_form_locations();
                    }
                    if ui.button("File…").clicked() {
                        self.begin_pick(
                            PickTarget::Input,
                            PickMode::File,
                            self.create.input.clone(),
                            false,
                        );
                    }
                    if ui.button("Folder…").clicked() {
                        self.begin_pick(
                            PickTarget::Input,
                            PickMode::Folder,
                            self.create.input.clone(),
                            false,
                        );
                    }
                });
                ui.add_space(12.0);
                label(ui, "Save torrent to");
                ui.horizontal(|ui| {
                    let response = ui.add(
                        TextEdit::singleline(&mut self.create.output)
                            .hint_text("Output .torrent file")
                            .desired_width((ui.available_width() - 110.0).max(100.0)),
                    );
                    if response.lost_focus() {
                        self.remember_form_locations();
                    }
                    if ui.button("Browse…").clicked() {
                        self.begin_pick(
                            PickTarget::Output,
                            PickMode::Save,
                            self.create.output.clone(),
                            true,
                        );
                    }
                });
                ui.add_space(10.0);
                let mut enabled = self.preferences.remember_locations;
                if ui
                    .add(egui::Checkbox::new(
                        &mut enabled,
                        RichText::new("Remember locations").size(12.0),
                    ))
                    .changed()
                {
                    if let Err(error) = self.preferences.set_enabled(enabled) {
                        self.notice = Some((true, format!("{error:#}")));
                    } else if enabled {
                        self.remember_form_locations();
                    }
                }
            });
            ui.add_space(14.0);
            card(ui, |ui| {
                section(ui, "Format & discovery");
                ui.columns(2, |cols| {
                    label(&mut cols[0], "Protocol");
                    cols[0].horizontal(|ui| {
                        ui.selectable_value(&mut self.create.protocol, Protocol::V1, "v1");
                        ui.selectable_value(&mut self.create.protocol, Protocol::V2, "v2");
                        ui.selectable_value(&mut self.create.protocol, Protocol::Hybrid, "Hybrid");
                    });
                    label(&mut cols[1], "Piece size");
                    egui::ComboBox::from_id_salt("piece-size")
                        .width((cols[1].available_width() - 16.0).max(100.0))
                        .selected_text(piece_name(self.create.piece_length))
                        .show_ui(&mut cols[1], |ui| {
                            ui.selectable_value(
                                &mut self.create.piece_length,
                                None,
                                "Auto (recommended)",
                            );
                            for power in 14..=26 {
                                let bytes = 1usize << power;
                                ui.selectable_value(
                                    &mut self.create.piece_length,
                                    Some(bytes),
                                    piece_name(Some(bytes)),
                                );
                            }
                        });
                });
                ui.add_space(8.0);
                small(
                    ui,
                    match self.create.protocol {
                        Protocol::V1 => {
                            "v1 offers broad compatibility. Auto chooses 16 KiB–16 MiB pieces."
                        }
                        Protocol::V2 => {
                            "v2 uses SHA-256 file trees. Auto chooses 16 KiB–16 MiB pieces."
                        }
                        Protocol::Hybrid => {
                            "Hybrid includes v1 and v2 hashes. Progress shows each pass separately."
                        }
                    },
                );
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    let old = self.create.private;
                    ui.selectable_value(&mut self.create.private, true, "Private");
                    ui.selectable_value(&mut self.create.private, false, "Public");
                    if !old && self.create.private {
                        self.create.dht_nodes.clear();
                    }
                    ui.label(
                        RichText::new(if self.create.private {
                            "Tracker only · DHT disabled"
                        } else {
                            "DHT & peer discovery enabled"
                        })
                        .size(12.0)
                        .color(MUTED),
                    );
                });
                ui.add_space(14.0);
                let recent_trackers = self.preferences.recent_private_trackers().to_vec();
                let mut selected_recent = None;
                let mut clear_recent = false;
                ui.horizontal(|ui| {
                    label(ui, "Tracker announce URLs");
                    if self.create.private {
                        if !recent_trackers.is_empty() {
                            egui::ComboBox::from_id_salt("recent-private-trackers")
                                .selected_text("Recent trackers")
                                .show_ui(ui, |ui| {
                                    for url in &recent_trackers {
                                        if ui.selectable_label(false, tracker_label(url)).clicked()
                                        {
                                            selected_recent = Some(url.clone());
                                        }
                                    }
                                    ui.separator();
                                    if ui.button("Clear history").clicked() {
                                        clear_recent = true;
                                    }
                                });
                        }
                    } else if ui.button("Add public tracker").clicked() {
                        add_tracker(&mut self.create.trackers, PUBLIC_TRACKER);
                    }
                });
                if let Some(url) = selected_recent {
                    add_tracker(&mut self.create.trackers, &url);
                }
                if clear_recent && let Err(error) = self.preferences.clear_recent_private_trackers()
                {
                    self.notice = Some((true, format!("{error:#}")));
                }
                ui.add(
                    TextEdit::multiline(&mut self.create.trackers)
                        .hint_text(
                            "https://tracker.example/announce\nudp://tracker.example:1337/announce",
                        )
                        .desired_rows(3)
                        .desired_width(f32::INFINITY),
                );
                small(
                    ui,
                    "One URL per line. Separate tracker tiers with a blank line.",
                );
            });
            ui.add_space(14.0);
            card(ui, |ui| {
                egui::CollapsingHeader::new(RichText::new("More options").strong())
                    .id_salt("create-advanced")
                    .show(ui, |ui| {
                        ui.add_space(10.0);
                        ui.columns(2, |cols| {
                            field(
                                &mut cols[0],
                                "Torrent name",
                                &mut self.create.name,
                                "Use the payload name",
                            );
                            field(
                                &mut cols[1],
                                "Source",
                                &mut self.create.source,
                                "Optional source tag",
                            );
                        });
                        field(
                            ui,
                            "Comment",
                            &mut self.create.comment,
                            "Optional description",
                        );
                        ui.columns(2, |cols| {
                            field(&mut cols[0], "Workers", &mut self.create.workers, "auto");
                            label(&mut cols[1], "Read method");
                            egui::ComboBox::from_id_salt("io-mode")
                                .selected_text(format!("{:?}", self.create.io))
                                .show_ui(&mut cols[1], |ui| {
                                    for mode in [
                                        IoMode::Auto,
                                        IoMode::Mmap,
                                        IoMode::Buffered,
                                        IoMode::Direct,
                                    ] {
                                        ui.selectable_value(
                                            &mut self.create.io,
                                            mode,
                                            format!("{mode:?}"),
                                        );
                                    }
                                });
                        });
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut self.create.include_hidden, "Include hidden files");
                            ui.checkbox(&mut self.create.follow_symlinks, "Follow symbolic links");
                        });
                        label(ui, "Per-file checksums");
                        ui.horizontal(|ui| {
                            for algorithm in ["sha1", "sha256", "sha512", "md5"] {
                                let mut enabled =
                                    self.create.checksums.iter().any(|a| a == algorithm);
                                if ui
                                    .checkbox(&mut enabled, algorithm.to_uppercase())
                                    .changed()
                                {
                                    self.create.checksums.retain(|a| a != algorithm);
                                    if enabled {
                                        self.create.checksums.push(algorithm.into());
                                    }
                                }
                            }
                        });
                        label(ui, "Web seeds");
                        ui.add(
                            TextEdit::multiline(&mut self.create.web_seeds)
                                .hint_text("https://example.com/files/")
                                .desired_rows(2)
                                .desired_width(f32::INFINITY),
                        );
                        ui.add_space(10.0);
                        ui.add_enabled_ui(!self.create.private, |ui| {
                            label(ui, "DHT bootstrap nodes (optional)");
                            ui.add(
                                TextEdit::multiline(&mut self.create.dht_nodes)
                                    .hint_text("hostname:port · one node per line")
                                    .desired_rows(2)
                                    .desired_width(f32::INFINITY),
                            );
                            small(
                                ui,
                                "Public torrents can use DHT without adding bootstrap nodes here.",
                            );
                        });
                    });
            });
            ui.add_space(16.0);
        });
    }

    fn edit_ui(&mut self, ui: &mut egui::Ui) {
        hero(
            ui,
            "Edit a torrent",
            "Refine metadata or inspect every field. Save when you're ready.",
        );
        ui.horizontal(|ui| {
            if ui
                .add_enabled(self.job.is_none(), egui::Button::new("Open torrent…"))
                .clicked()
            {
                let path = self
                    .editor
                    .as_ref()
                    .map(|e| e.path.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.begin_pick(PickTarget::Open, PickMode::File, path, true);
            }
            if let Some(editor) = &self.editor {
                ui.label(
                    RichText::new(
                        editor
                            .path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy(),
                    )
                    .color(MUTED),
                );
            }
        });
        ui.add_space(16.0);
        let Some(editor) = self.editor.as_mut() else {
            card(ui, |ui| {
                ui.add_space(35.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("Everything in one place").size(22.0).strong());
                    ui.add_space(9.0);
                    small(
                        ui,
                        "Open a .torrent to edit trackers, privacy, names, and custom fields.",
                    );
                    small(
                        ui,
                        "Unknown fields and existing piece hashes are preserved.",
                    );
                });
                ui.add_space(35.0);
            });
            return;
        };
        let mut notice = None;
        let mut remove = None;
        ui.add_enabled_ui(self.job.is_none(), |ui| {
            ui.horizontal(|ui| {
                let mut tree = editor.tree;
                ui.selectable_value(&mut tree, false, "Common fields");
                ui.selectable_value(&mut tree, true, "Field tree");
                if tree != editor.tree {
                    match editor.apply_form() {
                        Ok(()) => editor.tree = tree,
                        Err(error) => notice = Some((true, format!("{error:#}"))),
                    }
                }
                if editor.changed() {
                    ui.label(RichText::new("Unsaved changes").color(ACCENT).size(12.0));
                }
            });
            ui.add_space(13.0);
            if editor.tree {
                tree_ui(ui, editor, &mut notice, &mut remove);
            } else {
                card(ui, |ui| {
                    ui.columns(2, |cols| {
                        field(&mut cols[0], "Torrent name", &mut editor.form.name, "");
                        field(&mut cols[1], "Source", &mut editor.form.source, "");
                    });
                    let old = editor.form.private;
                    ui.checkbox(
                        &mut editor.form.private,
                        "Private torrent (disable DHT and peer discovery)",
                    );
                    if !old && editor.form.private {
                        editor.form.dht_nodes.clear();
                    }
                    field(ui, "Comment", &mut editor.form.comment, "");
                    ui.columns(2, |cols| {
                        field(&mut cols[0], "Created by", &mut editor.form.created_by, "");
                        field(
                            &mut cols[1],
                            "Creation date",
                            &mut editor.form.creation_date,
                            "Unix timestamp, or blank to remove",
                        );
                    });
                    label(ui, "Tracker announce URLs");
                    ui.add(
                        TextEdit::multiline(&mut editor.form.trackers)
                            .desired_rows(4)
                            .desired_width(f32::INFINITY),
                    );
                    small(ui, "One URL per line; a blank line starts another tier.");
                    label(ui, "Web seeds");
                    ui.add(
                        TextEdit::multiline(&mut editor.form.web_seeds)
                            .desired_rows(2)
                            .desired_width(f32::INFINITY),
                    );
                    ui.add_enabled_ui(!editor.form.private, |ui| {
                        label(ui, "DHT bootstrap nodes");
                        ui.add(
                            TextEdit::multiline(&mut editor.form.dht_nodes)
                                .hint_text("hostname:port")
                                .desired_rows(2)
                                .desired_width(f32::INFINITY),
                        );
                    });
                });
            }
            ui.add_space(15.0);
        });
        if let Some(notice) = notice {
            self.notice = Some(notice);
        }
        if let Some(pointer) = remove {
            self.confirm = Some(Confirm::Remove(pointer));
        }
    }

    fn action_bar(&mut self, ui: &mut egui::Ui) {
        if let Some(job) = &self.job {
            let status = job.progress.status.lock().unwrap();
            ui.horizontal(|ui| {
                ui.label(RichText::new(status.phase).strong().color(ACCENT));
                if let Some(plan) = &status.plan {
                    let bytes = job.progress.bytes.load(Ordering::Relaxed).min(plan.total);
                    let percent = if bytes == plan.total {
                        100.0
                    } else {
                        (bytes as f64 / plan.total as f64 * 100.0).min(99.9)
                    };
                    let elapsed = status.since.elapsed().as_secs_f64();
                    let rate = bytes as f64 / elapsed.max(0.001);
                    let eta = if bytes == plan.total {
                        "00:00".into()
                    } else if rate > 0.0 {
                        format_duration(((plan.total - bytes) as f64 / rate).ceil() as u64)
                    } else {
                        "--:--".into()
                    };
                    ui.label(
                        RichText::new(format!(
                            "{} · {percent:.1}% · {}/{} · {}/s · ETA {eta}",
                            protocol_name(plan.protocol),
                            size(bytes as f64),
                            size(plan.total as f64),
                            size(rate)
                        ))
                        .size(12.0)
                        .color(MUTED),
                    );
                }
            });
            let fraction = status
                .plan
                .as_ref()
                .map(|p| {
                    if p.total == 0 {
                        1.0
                    } else {
                        job.progress.bytes.load(Ordering::Relaxed) as f32 / p.total as f32
                    }
                })
                .unwrap_or(0.0)
                .min(1.0);
            ui.add_space(6.0);
            ui.add(
                egui::ProgressBar::new(fraction)
                    .desired_width(ui.available_width())
                    .fill(ACCENT),
            );
            if let Some(plan) = &status.plan {
                small(
                    ui,
                    &format!(
                        "{} pieces · {} workers · {}/{}",
                        size(plan.pieces as f64),
                        plan.workers,
                        plan.hash,
                        plan.io
                    ),
                );
            }
            return;
        }
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new(if self.created.is_some() && self.tab == Tab::Create {
                        "Torrent created"
                    } else {
                        "Ready when you are"
                    })
                    .strong(),
                );
                if self.tab == Tab::Edit {
                    small(ui, "Metadata edits keep payload hashes intact");
                }
            });
            ui.with_layout(
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| match self.tab {
                    Tab::Create => {
                        if primary(ui, "Create torrent").clicked() {
                            self.prepare_create();
                        }
                        if let Some((path, _)) = &self.created {
                            let path = path.clone();
                            if ui.button("Open in editor").clicked() {
                                self.request_open(path);
                            }
                        }
                    }
                    Tab::Edit => {
                        if ui
                            .add_enabled(
                                self.editor.is_some(),
                                egui::Button::new(RichText::new("Save changes").color(BG).strong())
                                    .fill(ACCENT)
                                    .min_size(Vec2::new(130.0, 37.0)),
                            )
                            .clicked()
                            && let Some(editor) = &self.editor
                        {
                            self.prepare_save(editor.path.clone());
                        }
                        if ui
                            .add_enabled(self.editor.is_some(), egui::Button::new("Save as…"))
                            .clicked()
                        {
                            let path = self
                                .editor
                                .as_ref()
                                .unwrap()
                                .path
                                .to_string_lossy()
                                .into_owned();
                            self.begin_pick(PickTarget::SaveAs, PickMode::Save, path, true);
                        }
                    }
                },
            );
        });
    }

    fn suggest_output(&mut self) {
        if self.create.output.trim().is_empty()
            && let Ok(path) = model::expand_path(&self.create.input)
            && let Some(name) = path.file_name()
        {
            let mut filename = name.to_os_string();
            filename.push(".torrent");
            self.create.output = self
                .preferences
                .directory(LocationKind::Output)
                .or_else(|| path.parent())
                .unwrap_or_else(|| std::path::Path::new("."))
                .join(filename)
                .to_string_lossy()
                .into_owned();
        }
    }
    fn begin_pick(
        &mut self,
        target: PickTarget,
        mode: PickMode,
        current: String,
        torrent_only: bool,
    ) {
        let kind = match target {
            PickTarget::Input => LocationKind::Input,
            PickTarget::Output | PickTarget::SaveAs => LocationKind::Output,
            PickTarget::Open => LocationKind::Torrent,
        };
        let remembered_save = (matches!(target, PickTarget::SaveAs))
            .then(|| self.preferences.directory(LocationKind::Output))
            .flatten()
            .and_then(|directory| {
                model::expand_path(&current)
                    .ok()
                    .and_then(|path| path.file_name().map(|name| directory.join(name)))
            });
        let seed = remembered_save
            .or_else(|| {
                model::expand_path(&current).ok().filter(|path| {
                    path.is_dir() || path.parent().is_some_and(|parent| parent.is_dir())
                })
            })
            .unwrap_or_else(|| {
                let directory = self
                    .preferences
                    .directory(kind)
                    .map(PathBuf::from)
                    .or_else(|| {
                        std::env::var_os("USERPROFILE")
                            .or_else(|| std::env::var_os("HOME"))
                            .map(PathBuf::from)
                    })
                    .unwrap_or_else(|| PathBuf::from("."));
                if mode == PickMode::Save {
                    let filename = model::expand_path(&current)
                        .ok()
                        .and_then(|path| path.file_name().map(|name| name.to_os_string()));
                    directory.join(filename.unwrap_or_else(|| "untitled.torrent".into()))
                } else {
                    directory
                }
            });
        self.picker = Some((
            target,
            Picker::new(mode, &seed.to_string_lossy(), torrent_only),
        ));
    }
    fn remember_location(&mut self, kind: LocationKind, path: &std::path::Path) {
        if let Err(error) = self.preferences.record(kind, path) {
            self.notice = Some((true, format!("{error:#}")));
        }
    }
    fn remember_form_locations(&mut self) {
        if !self.preferences.remember_locations {
            return;
        }
        if let Ok(path) = model::expand_path(&self.create.input)
            && path.exists()
        {
            self.remember_location(LocationKind::Input, &path);
        }
        if let Ok(path) = model::expand_path(&self.create.output)
            && path
                .parent()
                .is_some_and(|parent| parent.is_dir() || parent.as_os_str().is_empty())
        {
            self.remember_location(LocationKind::Output, &path);
        }
        if let Some(path) = self.editor.as_ref().map(|editor| editor.path.clone()) {
            self.remember_location(LocationKind::Torrent, &path);
        }
    }
    fn picker_ui(&mut self, ctx: &egui::Context) {
        let Some((target, picker)) = &mut self.picker else {
            return;
        };
        let result = picker.ui(ctx);
        let target = *target;
        let (open, selected) = match result {
            Ok(result) => result,
            Err(error) => {
                self.picker = None;
                self.notice = Some((true, format!("{error:#}")));
                return;
            }
        };
        if !open {
            self.picker = None;
        }
        if let Some(path) = selected {
            match target {
                PickTarget::Input => {
                    self.remember_location(LocationKind::Input, &path);
                    self.create.input = path.to_string_lossy().into_owned();
                    self.suggest_output();
                }
                PickTarget::Output => {
                    self.remember_location(LocationKind::Output, &path);
                    self.create.output = path.to_string_lossy().into_owned();
                }
                PickTarget::Open => {
                    self.remember_location(LocationKind::Torrent, &path);
                    if self.editor.as_ref().is_some_and(Editor::changed) {
                        self.confirm = Some(Confirm::DiscardOpen(path));
                    } else {
                        self.open(path);
                    }
                }
                PickTarget::SaveAs => {
                    self.remember_location(LocationKind::Output, &path);
                    self.prepare_save(path);
                }
            }
        }
    }
    fn prepare_create(&mut self) {
        self.suggest_output();
        match self.create.options() {
            Ok(options) if options.output.as_ref().is_some_and(|p| p.exists()) => {
                self.confirm = Some(Confirm::Create(Box::new(options)))
            }
            Ok(options) => self.start_create(options, false),
            Err(error) => self.notice = Some((true, format!("{error:#}"))),
        }
    }
    fn prepare_save(&mut self, path: PathBuf) {
        let Some(editor) = &mut self.editor else {
            return;
        };
        match editor.apply_form() {
            Ok(()) => {
                let doc = editor.document.clone();
                if path.exists() {
                    self.confirm = Some(Confirm::Save(path, doc));
                } else {
                    self.save(path, doc, false);
                }
            }
            Err(error) => self.notice = Some((true, format!("{error:#}"))),
        }
    }
    fn launch(
        &mut self,
        phase: &'static str,
        task: impl FnOnce(Arc<WorkProgress>) -> Result<Outcome> + Send + 'static,
    ) {
        let progress = Arc::new(WorkProgress::new(phase));
        let thread_progress = progress.clone();
        let (sender, receiver) = mpsc::channel();
        match thread::Builder::new()
            .name("rustide-task".into())
            .spawn(move || {
                let _ = sender.send(task(thread_progress));
            }) {
            Ok(_) => {
                self.job = Some(Job { receiver, progress });
                self.notice = None;
            }
            Err(error) => self.notice = Some((true, format!("Could not start task: {error}"))),
        }
    }
    fn start_create(&mut self, options: CreateOptions, overwrite: bool) {
        self.created = None;
        self.launch("Scanning files", move |progress| {
            let private_trackers = if options.private == Some(true) {
                options.announces.iter().flatten().cloned().collect()
            } else {
                Vec::new()
            };
            let output = options.output.clone().context("choose an output path")?;
            let result = create::create_with_progress(&options, &|event| progress.event(event))?;
            let document = metadata::from_value(result.value)?;
            progress.phase("Writing torrent");
            model::validate_create_paths(&options.target, &output)?;
            model::save_document(&document, &output, overwrite)?;
            Ok(Outcome::Created(output, result.report, private_trackers))
        });
    }
    fn open(&mut self, path: PathBuf) {
        self.tab = Tab::Edit;
        self.launch("Opening torrent", move |_| {
            Ok(Outcome::Opened(path.clone(), metadata::load(&path)?))
        });
    }
    fn request_open(&mut self, path: PathBuf) {
        if self.editor.as_ref().is_some_and(Editor::changed) {
            self.confirm = Some(Confirm::DiscardOpen(path));
        } else {
            self.open(path);
        }
    }
    fn save(&mut self, path: PathBuf, document: Document, overwrite: bool) {
        self.launch("Saving torrent", move |_| {
            model::save_document(&document, &path, overwrite)?;
            Ok(Outcome::Saved(path, document))
        });
    }
    fn poll(&mut self) {
        let result = match self.job.as_ref().map(|j| j.receiver.try_recv()) {
            Some(Ok(result)) => result,
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                Err(anyhow::anyhow!("The task stopped unexpectedly"))
            }
            _ => return,
        };
        self.job = None;
        match result {
            Ok(Outcome::Created(path, report, private_trackers)) => {
                self.remember_form_locations();
                let mut message = format!(
                    "Created {} · {} files · {} · {:.2}s",
                    path.file_name().unwrap_or_default().to_string_lossy(),
                    report.files,
                    size(report.payload_bytes as f64),
                    report.total_seconds
                );
                let error = self
                    .preferences
                    .record_private_trackers(&private_trackers)
                    .err();
                if let Some(error) = &error {
                    message.push_str(&format!(" · Could not remember tracker: {error:#}"));
                }
                self.notice = Some((error.is_some(), message));
                self.created = Some((path, report));
            }
            Ok(Outcome::Opened(path, document) | Outcome::Saved(path, document)) => {
                self.remember_location(LocationKind::Torrent, &path);
                self.editor = Some(Editor::new(path, document));
                self.notice = Some((
                    false,
                    "Torrent ready. All fields are available in the editor.".into(),
                ));
            }
            Err(error) => self.notice = Some((true, format!("{error:#}"))),
        }
    }
    fn confirm_ui(&mut self, ctx: &egui::Context) {
        let Some(confirm) = &self.confirm else { return };
        let (title, message, accept) = match confirm {
            Confirm::Create(options) => (
                "Replace existing torrent?",
                format!(
                    "{} already exists. Replace it with the newly created torrent?",
                    options.output.as_ref().unwrap().display()
                ),
                "Replace & create",
            ),
            Confirm::Save(path, _) => (
                "Replace existing torrent?",
                format!("Save your changes to {}?", path.display()),
                "Save & replace",
            ),
            Confirm::DiscardOpen(_) => (
                "Discard unsaved changes?",
                "Your current edits have not been saved. Open another torrent?".into(),
                "Discard & open",
            ),
            Confirm::Close => (
                "Close Rustide?",
                if self.job.is_some() {
                    "A task is still running. Closing will stop it.".into()
                } else {
                    "Your torrent has unsaved changes.".into()
                },
                "Close",
            ),
            Confirm::Remove(pointer) => (
                "Remove this field?",
                format!(
                    "Remove {pointer} from this torrent? Changes are only written when you save."
                ),
                "Remove field",
            ),
        };
        let mut approved = false;
        let mut cancelled = false;
        egui::Modal::new(egui::Id::new("confirmation")).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.label(RichText::new(title).size(20.0).strong());
            ui.add_space(12.0);
            ui.label(message);
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                approved = primary(ui, accept).clicked();
                cancelled = ui.button("Cancel").clicked();
            });
        });
        if cancelled || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.confirm = None;
        }
        if approved {
            match self.confirm.take().unwrap() {
                Confirm::Create(options) => self.start_create(*options, true),
                Confirm::Save(path, document) => self.save(path, document, true),
                Confirm::DiscardOpen(path) => self.open(path),
                Confirm::Close => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                Confirm::Remove(pointer) => {
                    if let Some(editor) = &mut self.editor {
                        match model::remove_at(&editor.document, &pointer, editor.unlock_hashes) {
                            Ok(doc) => {
                                editor.replace_document(doc);
                                self.notice = Some((
                                    false,
                                    "Field removed. Save to write your changes.".into(),
                                ));
                            }
                            Err(error) => self.notice = Some((true, format!("{error:#}"))),
                        }
                    }
                }
            }
        }
    }
}

fn tree_ui(
    ui: &mut egui::Ui,
    editor: &mut Editor,
    notice: &mut Option<(bool, String)>,
    remove: &mut Option<String>,
) {
    card(ui, |ui| {
        ui.add(
            TextEdit::singleline(&mut editor.filter)
                .hint_text("Search fields or paths…")
                .desired_width(f32::INFINITY),
        );
        ui.add_space(8.0);
        let filter = editor.filter.to_lowercase();
        let rows: Vec<usize> = editor
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                filter.is_empty()
                    || r.pointer.to_lowercase().contains(&filter)
                    || r.label.to_lowercase().contains(&filter)
            })
            .map(|(i, _)| i)
            .collect();
        let mut selection = None;
        ScrollArea::vertical()
            .id_salt("field-tree")
            .max_height(260.0)
            .show_rows(ui, 25.0, rows.len(), |ui, range| {
                for index in range {
                    let row = &editor.rows[rows[index]];
                    ui.horizontal(|ui| {
                        ui.add_space(row.depth.min(8) as f32 * 12.0);
                        let display = if row.pointer.is_empty() {
                            "Torrent"
                        } else {
                            &row.label
                        };
                        if ui
                            .selectable_label(
                                editor.selected == row.pointer,
                                RichText::new(display).color(if row.protected {
                                    MUTED
                                } else {
                                    TEXT
                                }),
                            )
                            .clicked()
                        {
                            selection = Some(row.pointer.clone());
                        }
                        ui.label(RichText::new(row.kind).size(10.0).color(ACCENT));
                        ui.add(
                            egui::Label::new(RichText::new(&row.preview).size(11.0).color(MUTED))
                                .truncate(),
                        );
                    });
                }
            });
        if let Some(pointer) = selection {
            editor.selected = pointer;
            match metadata::query(&editor.document.value, &editor.selected) {
                Ok(value) => match model::field_json(value) {
                    Ok(text) => editor.value = text,
                    Err(error) => {
                        editor.value.clear();
                        *notice = Some((true, format!("{error:#}")));
                    }
                },
                Err(error) => *notice = Some((true, format!("{error:#}"))),
            }
        }
    });
    ui.add_space(13.0);
    card(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Selected field").strong());
            ui.label(
                RichText::new(if editor.selected.is_empty() {
                    "/"
                } else {
                    &editor.selected
                })
                .monospace()
                .color(MUTED),
            );
        });
        ui.add_space(8.0);
        ui.add(
            TextEdit::multiline(&mut editor.value)
                .code_editor()
                .desired_rows(6)
                .desired_width(f32::INFINITY),
        );
        small(
            ui,
            "Text: \"value\"   Integer: 42   Bytes: {\"$bytes\":\"00ff\"}   List: []   Dictionary: {}",
        );
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !editor.value.is_empty(),
                    egui::Button::new("Apply field change"),
                )
                .clicked()
            {
                match model::replace_at(
                    &editor.document,
                    &editor.selected,
                    &editor.value,
                    editor.unlock_hashes,
                ) {
                    Ok(document) => {
                        editor.replace_document(document);
                        *notice =
                            Some((false, "Field updated. Save to write your changes.".into()));
                    }
                    Err(error) => *notice = Some((true, format!("{error:#}"))),
                }
            }
            if ui
                .add_enabled(
                    !editor.selected.is_empty(),
                    egui::Button::new("Remove field…"),
                )
                .clicked()
            {
                *remove = Some(editor.selected.clone());
            }
        });
        ui.add_space(14.0);
        egui::CollapsingHeader::new("Add a child field").show(ui, |ui| {
            field(
                ui,
                "Dictionary key or list index",
                &mut editor.add_key,
                "New field name",
            );
            ui.add(
                TextEdit::multiline(&mut editor.add_value)
                    .code_editor()
                    .desired_rows(3)
                    .desired_width(f32::INFINITY),
            );
            if ui.button("Add to selected field").clicked() {
                match model::add_at(
                    &editor.document,
                    &editor.selected,
                    &editor.add_key,
                    &editor.add_value,
                    editor.unlock_hashes,
                ) {
                    Ok(document) => {
                        editor.replace_document(document);
                        *notice = Some((false, "Field added. Save to write your changes.".into()));
                    }
                    Err(error) => *notice = Some((true, format!("{error:#}"))),
                }
            }
        });
        ui.add_space(12.0);
        ui.checkbox(
            &mut editor.unlock_hashes,
            "Allow explicit changes to piece hashes",
        );
        if editor.unlock_hashes {
            ui.label(
                RichText::new(
                    "Changing piece hashes can invalidate this torrent. No payload is rehashed.",
                )
                .size(12.0)
                .color(ERROR),
            );
        } else {
            small(
                ui,
                "Piece hashes are protected. Metadata edits preserve unknown fields.",
            );
        }
    });
}

fn style(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::light();
    visuals.override_text_color = Some(TEXT);
    visuals.weak_text_color = Some(MUTED);
    visuals.panel_fill = BG;
    visuals.window_fill = PANEL;
    visuals.window_stroke = Stroke::new(1.0_f32, BORDER);
    visuals.window_corner_radius = 16.into();
    visuals.extreme_bg_color = BG;
    visuals.text_edit_bg_color = Some(Color32::from_rgb(255, 253, 249));
    visuals.selection.bg_fill = Color32::from_rgb(229, 201, 203);
    visuals.selection.stroke = Stroke::new(1.0_f32, ACCENT);
    visuals.hyperlink_color = ACCENT;
    visuals.error_fg_color = ERROR;
    for widget in [
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.noninteractive,
    ] {
        widget.corner_radius = 9.into();
        widget.bg_stroke = Stroke::new(1.0_f32, BORDER);
    }
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(242, 233, 225);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(234, 216, 209);
    visuals.widgets.active.bg_fill = Color32::from_rgb(220, 193, 191);
    ctx.set_visuals(visuals);
    ctx.style_mut(|style| {
        style.spacing.item_spacing = Vec2::new(10.0, 8.0);
        style.spacing.button_padding = Vec2::new(12.0, 8.0);
        style.spacing.interact_size.y = 32.0;
        style
            .text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(14.0));
        style
            .text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(14.0));
        style
            .text_styles
            .insert(egui::TextStyle::Small, egui::FontId::proportional(12.0));
    });
}
fn card(ui: &mut egui::Ui, content: impl FnOnce(&mut egui::Ui)) {
    Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .corner_radius(16)
        .inner_margin(18)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            content(ui);
        });
}
fn hero(ui: &mut egui::Ui, title: &str, description: &str) {
    ui.label(RichText::new(title).size(29.0).strong());
    ui.add_space(3.0);
    small(ui, description);
    ui.add_space(22.0);
}
fn section(ui: &mut egui::Ui, title: &str) {
    ui.label(RichText::new(title).size(16.0).strong());
    ui.add_space(10.0);
}
fn label(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).size(12.0).color(MUTED));
}
fn small(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).size(12.0).color(MUTED));
}
fn field(ui: &mut egui::Ui, title: &str, value: &mut String, hint: &str) {
    label(ui, title);
    ui.add(
        TextEdit::singleline(value)
            .hint_text(hint)
            .desired_width(f32::INFINITY),
    );
    ui.add_space(8.0);
}
fn primary(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(text).color(BG).strong())
            .fill(ACCENT)
            .stroke(Stroke::NONE)
            .corner_radius(10)
            .min_size(Vec2::new(133.0, 38.0)),
    )
}
fn add_tracker(text: &mut String, url: &str) {
    if !text.lines().any(|line| line.trim() == url) {
        if !text.trim().is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(url);
    }
}
fn tracker_label(url: &str) -> String {
    let address = url.split_once("://").map_or(url, |(_, address)| address);
    let host = address
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(address)
        .rsplit('@')
        .next()
        .unwrap_or(address);
    let mut label = host.chars().take(32).collect::<String>();
    if host.chars().count() > 32 {
        label.push('…');
    }
    label
}
fn protocol_name(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::V1 => "v1",
        Protocol::V2 => "v2",
        Protocol::Hybrid => "hybrid",
    }
}
fn piece_name(bytes: Option<usize>) -> String {
    match bytes {
        None => "Auto (recommended)".into(),
        Some(bytes) => size(bytes as f64),
    }
}
fn size(bytes: f64) -> String {
    let mut value = bytes;
    let mut unit = "B";
    for next in ["KiB", "MiB", "GiB", "TiB"] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    if unit == "B" || value.fract() == 0.0 {
        format!("{value:.0} {unit}")
    } else {
        format!("{value:.1} {unit}")
    }
}
fn format_duration(seconds: u64) -> String {
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    }
}

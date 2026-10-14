use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

const MAX_SETTINGS_BYTES: u64 = 64 * 1024;
const MAX_RECENT_PRIVATE_TRACKERS: usize = 5;
const MAX_TRACKER_BYTES: usize = 4096;

#[derive(Clone, Copy)]
pub(super) enum LocationKind {
    Input,
    Output,
    Torrent,
}

#[derive(Clone)]
pub(super) struct Preferences {
    pub remember_locations: bool,
    directories: Directories,
    recent_private_trackers: Vec<String>,
    path: Option<PathBuf>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
struct Directories {
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    torrent: Option<PathBuf>,
}

impl Directories {
    fn is_empty(&self) -> bool {
        self.input.is_none() && self.output.is_none() && self.torrent.is_none()
    }

    fn slot(&self, kind: LocationKind) -> &Option<PathBuf> {
        match kind {
            LocationKind::Input => &self.input,
            LocationKind::Output => &self.output,
            LocationKind::Torrent => &self.torrent,
        }
    }

    fn slot_mut(&mut self, kind: LocationKind) -> &mut Option<PathBuf> {
        match kind {
            LocationKind::Input => &mut self.input,
            LocationKind::Output => &mut self.output,
            LocationKind::Torrent => &mut self.torrent,
        }
    }
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct StoredPreferences {
    remember_locations: bool,
    #[serde(skip_serializing_if = "Directories::is_empty")]
    directories: Directories,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    recent_private_trackers: Vec<String>,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            remember_locations: false,
            directories: Directories::default(),
            recent_private_trackers: Vec::new(),
            path: settings_path(),
        }
    }
}

impl Preferences {
    pub fn load() -> Result<Self> {
        let Some(path) = settings_path() else {
            return Ok(Self::default());
        };
        Self::load_from(path)
    }

    fn load_from(path: PathBuf) -> Result<Self> {
        let mut preferences = Self {
            path: Some(path.clone()),
            ..Self::default()
        };
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(preferences);
            }
            Err(error) => return Err(error).context("cannot read GUI settings"),
        };
        let mut bytes = Vec::new();
        file.take(MAX_SETTINGS_BYTES + 1)
            .read_to_end(&mut bytes)
            .context("cannot read GUI settings")?;
        ensure!(
            bytes.len() as u64 <= MAX_SETTINGS_BYTES,
            "GUI settings exceed 64 KiB"
        );
        let stored: StoredPreferences =
            serde_json::from_slice(&bytes).context("GUI settings must be valid JSON")?;
        preferences.remember_locations = stored.remember_locations;
        for url in stored.recent_private_trackers {
            if valid_private_tracker(&url)
                && !preferences.recent_private_trackers.contains(&url)
                && preferences.recent_private_trackers.len() < MAX_RECENT_PRIVATE_TRACKERS
            {
                preferences.recent_private_trackers.push(url);
            }
        }
        if stored.remember_locations {
            preferences.directories = stored.directories;
            for kind in [
                LocationKind::Input,
                LocationKind::Output,
                LocationKind::Torrent,
            ] {
                let slot = preferences.directories.slot_mut(kind);
                if slot
                    .as_deref()
                    .is_some_and(|path| !path.is_absolute() || !path.is_dir())
                {
                    *slot = None;
                }
            }
        }
        Ok(preferences)
    }

    pub fn directory(&self, kind: LocationKind) -> Option<&Path> {
        if !self.remember_locations {
            return None;
        }
        self.directories
            .slot(kind)
            .as_deref()
            .filter(|path| path.is_absolute() && path.is_dir())
    }

    pub fn recent_private_trackers(&self) -> &[String] {
        &self.recent_private_trackers
    }

    pub fn record_private_trackers(&mut self, urls: &[String]) -> Result<()> {
        if urls.is_empty() {
            return Ok(());
        }
        ensure!(
            urls.iter().all(|url| valid_private_tracker(url)),
            "private tracker URL is invalid"
        );
        let mut recent = self.recent_private_trackers.clone();
        for url in urls.iter().rev() {
            recent.retain(|saved| saved != url);
            recent.insert(0, url.clone());
        }
        recent.truncate(MAX_RECENT_PRIVATE_TRACKERS);
        if recent != self.recent_private_trackers {
            self.recent_private_trackers = recent;
            self.save()?;
        }
        Ok(())
    }

    pub fn clear_recent_private_trackers(&mut self) -> Result<()> {
        if !self.recent_private_trackers.is_empty() {
            self.recent_private_trackers.clear();
            self.save()?;
        }
        Ok(())
    }

    pub fn record(&mut self, kind: LocationKind, path: &Path) -> Result<()> {
        if !self.remember_locations {
            return Ok(());
        }
        let directory = if path.is_dir() {
            path
        } else {
            path.parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new("."))
        };
        let directory = directory
            .canonicalize()
            .context("cannot remember the selected folder")?;
        ensure!(directory.is_dir(), "selected folder no longer exists");
        *self.directories.slot_mut(kind) = Some(readable_path(directory));
        self.save()
    }

    pub fn set_enabled(&mut self, enabled: bool) -> Result<()> {
        self.remember_locations = enabled;
        if !enabled {
            self.directories = Directories::default();
            if self.path.as_deref().is_none_or(|path| !path.exists()) {
                return Ok(());
            }
        }
        self.save()
    }

    fn save(&self) -> Result<()> {
        let path = self
            .path
            .as_deref()
            .context("cannot locate the user configuration folder")?;
        let stored = StoredPreferences {
            remember_locations: self.remember_locations,
            directories: if self.remember_locations {
                self.directories.clone()
            } else {
                Directories::default()
            },
            recent_private_trackers: self.recent_private_trackers.clone(),
        };
        let mut bytes = serde_json::to_vec_pretty(&stored).context("cannot encode GUI settings")?;
        bytes.push(b'\n');
        ensure!(
            bytes.len() as u64 <= MAX_SETTINGS_BYTES,
            "GUI settings exceed 64 KiB"
        );
        let parent = path.parent().context("invalid GUI settings path")?;
        fs::create_dir_all(parent).context("cannot create the GUI settings folder")?;
        let mut temporary =
            tempfile::NamedTempFile::new_in(parent).context("cannot save GUI settings")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temporary
                .as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))
                .context("cannot protect GUI settings")?;
        }
        temporary
            .write_all(&bytes)
            .context("cannot save GUI settings")?;
        temporary
            .as_file()
            .sync_all()
            .context("cannot save GUI settings")?;
        temporary
            .persist(path)
            .map_err(|error| error.error)
            .context("cannot replace GUI settings")?;
        Ok(())
    }
}

fn valid_private_tracker(url: &str) -> bool {
    if url.len() > MAX_TRACKER_BYTES
        || url
            .chars()
            .any(|character| character.is_whitespace() || character == '\0')
    {
        return false;
    }
    ["https://", "http://", "udp://", "wss://", "ws://"]
        .iter()
        .find_map(|scheme| url.strip_prefix(scheme))
        .is_some_and(|address| {
            !address
                .split(['/', '?', '#'])
                .next()
                .unwrap_or_default()
                .is_empty()
        })
}

fn settings_path() -> Option<PathBuf> {
    let directory = |key| {
        std::env::var_os(key)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    #[cfg(target_os = "windows")]
    let root = directory("APPDATA")
        .or_else(|| directory("LOCALAPPDATA"))
        .or_else(|| directory("USERPROFILE").map(|path| path.join("AppData/Roaming")));
    #[cfg(not(target_os = "windows"))]
    let root =
        directory("XDG_CONFIG_HOME").or_else(|| directory("HOME").map(|path| path.join(".config")));
    root.map(|path| path.join("rustide/gui-settings.json"))
}

#[cfg(not(target_os = "windows"))]
fn readable_path(path: PathBuf) -> PathBuf {
    path
}

#[cfg(target_os = "windows")]
fn readable_path(path: PathBuf) -> PathBuf {
    use std::path::{Component, Prefix};

    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return path;
    };
    let mut readable = match prefix.kind() {
        Prefix::VerbatimDisk(drive) => PathBuf::from(format!("{}:\\", drive as char)),
        Prefix::VerbatimUNC(server, share) => {
            let mut root = PathBuf::from("\\\\");
            root.push(server);
            root.push(share);
            root
        }
        _ => return path,
    };
    for component in components {
        if component != Component::RootDir {
            readable.push(component.as_os_str());
        }
    }
    readable
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_in(directory: &Path) -> PathBuf {
        directory.join("config/rustide/gui-settings.json")
    }

    #[test]
    fn disabled_preferences_do_not_create_or_remember_anything() {
        let temporary = tempfile::tempdir().unwrap();
        let path = settings_in(temporary.path());
        let mut preferences = Preferences::load_from(path.clone()).unwrap();
        assert!(!preferences.remember_locations);
        preferences
            .record(LocationKind::Input, Path::new("missing-input"))
            .unwrap();
        preferences.set_enabled(false).unwrap();
        assert!(preferences.directory(LocationKind::Input).is_none());
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn opt_in_recalls_independent_folders_and_never_selected_filenames() {
        let temporary = tempfile::tempdir().unwrap();
        let path = settings_in(temporary.path());
        let mut preferences = Preferences::load_from(path.clone()).unwrap();
        preferences.set_enabled(true).unwrap();
        for (kind, name) in [
            (LocationKind::Input, "input"),
            (LocationKind::Output, "output"),
            (LocationKind::Torrent, "torrent"),
        ] {
            let directory = temporary.path().join(name);
            fs::create_dir(&directory).unwrap();
            let selected = directory.join("selected.torrent");
            fs::write(&selected, b"torrent contents remain private").unwrap();
            preferences.record(kind, &selected).unwrap();
        }
        let restored = Preferences::load_from(path.clone()).unwrap();
        assert!(restored.remember_locations);
        for (kind, name) in [
            (LocationKind::Input, "input"),
            (LocationKind::Output, "output"),
            (LocationKind::Torrent, "torrent"),
        ] {
            let expected = readable_path(temporary.path().join(name).canonicalize().unwrap());
            assert_eq!(restored.directory(kind), Some(expected.as_path()));
        }
        let stored = fs::read_to_string(path).unwrap();
        assert!(!stored.contains("selected.torrent"));
        assert!(!stored.contains("torrent contents"));
        let value: serde_json::Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 2);
        assert_eq!(
            value["directories"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["input", "output", "torrent"]
        );
    }

    #[test]
    fn disabling_immediately_erases_saved_folders() {
        let temporary = tempfile::tempdir().unwrap();
        let path = settings_in(temporary.path());
        let mut preferences = Preferences::load_from(path.clone()).unwrap();
        preferences.set_enabled(true).unwrap();
        preferences
            .record(LocationKind::Input, temporary.path())
            .unwrap();
        preferences.set_enabled(false).unwrap();
        let stored: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(stored, serde_json::json!({ "remember_locations": false }));
        preferences.set_enabled(true).unwrap();
        assert!(preferences.directory(LocationKind::Input).is_none());
        assert!(
            Preferences::load_from(path)
                .unwrap()
                .directory(LocationKind::Input)
                .is_none()
        );
    }

    #[test]
    fn ignores_missing_and_relative_directories_and_rechecks_moved_folders() {
        let temporary = tempfile::tempdir().unwrap();
        let path = settings_in(temporary.path());
        let folder = temporary.path().join("input");
        fs::create_dir(&folder).unwrap();
        let mut preferences = Preferences::load_from(path.clone()).unwrap();
        preferences.set_enabled(true).unwrap();
        preferences.record(LocationKind::Input, &folder).unwrap();
        fs::remove_dir(&folder).unwrap();
        assert!(preferences.directory(LocationKind::Input).is_none());
        assert!(
            Preferences::load_from(path.clone())
                .unwrap()
                .directory(LocationKind::Input)
                .is_none()
        );
        fs::write(
            &path,
            br#"{"remember_locations":true,"directories":{"input":".","output":"missing"}}"#,
        )
        .unwrap();
        let restored = Preferences::load_from(path).unwrap();
        assert!(restored.directory(LocationKind::Input).is_none());
        assert!(restored.directory(LocationKind::Output).is_none());
    }

    #[test]
    fn accepts_newer_settings_but_ignores_locations_when_disabled() {
        let temporary = tempfile::tempdir().unwrap();
        let path = settings_in(temporary.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "remember_locations": false,
                "directories": { "input": temporary.path() },
                "future_setting": { "enabled": true }
            }))
            .unwrap(),
        )
        .unwrap();
        let mut preferences = Preferences::load_from(path).unwrap();
        assert!(!preferences.remember_locations);
        preferences.set_enabled(true).unwrap();
        assert!(preferences.directory(LocationKind::Input).is_none());
    }

    #[test]
    fn rejects_corrupt_and_oversized_settings() {
        let temporary = tempfile::tempdir().unwrap();
        let path = settings_in(temporary.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{broken JSON").unwrap();
        assert!(Preferences::load_from(path.clone()).is_err());
        fs::write(&path, br#"{"remember_locations":"yes"}"#).unwrap();
        assert!(Preferences::load_from(path.clone()).is_err());
        fs::write(&path, vec![b' '; MAX_SETTINGS_BYTES as usize + 1]).unwrap();
        assert!(
            Preferences::load_from(path)
                .err()
                .unwrap()
                .to_string()
                .contains("64 KiB")
        );
    }

    #[cfg(unix)]
    #[test]
    fn saved_settings_are_only_readable_by_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let temporary = tempfile::tempdir().unwrap();
        let path = settings_in(temporary.path());
        let mut preferences = Preferences::load_from(path.clone()).unwrap();
        preferences.set_enabled(true).unwrap();
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn recent_private_trackers_are_bounded_reordered_and_clearable() {
        let temporary = tempfile::tempdir().unwrap();
        let path = settings_in(temporary.path());
        let mut preferences = Preferences::load_from(path.clone()).unwrap();
        let urls = (0..7)
            .map(|number| format!("https://tracker{number}.example/announce?key=sample"))
            .collect::<Vec<_>>();
        preferences.record_private_trackers(&urls).unwrap();
        assert_eq!(preferences.recent_private_trackers(), &urls[..5]);
        assert!(!preferences.remember_locations);
        preferences
            .record_private_trackers(&[urls[3].clone()])
            .unwrap();
        assert_eq!(preferences.recent_private_trackers()[0], urls[3]);
        assert_eq!(
            Preferences::load_from(path.clone())
                .unwrap()
                .recent_private_trackers(),
            preferences.recent_private_trackers()
        );
        assert!(
            preferences
                .record_private_trackers(&["invalid tracker".into()])
                .is_err()
        );
        preferences.clear_recent_private_trackers().unwrap();
        assert!(
            Preferences::load_from(path)
                .unwrap()
                .recent_private_trackers()
                .is_empty()
        );
    }

    #[test]
    fn invalid_saved_trackers_are_ignored() {
        let temporary = tempfile::tempdir().unwrap();
        let path = settings_in(temporary.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "recent_private_trackers": [
                    "https://tracker.example/announce",
                    "ftp://invalid.example/announce",
                    "https://tracker.example/announce",
                    "https://"
                ]
            }))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            Preferences::load_from(path)
                .unwrap()
                .recent_private_trackers(),
            &["https://tracker.example/announce"]
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn saved_windows_paths_work_with_standard_file_dialogs() {
        assert_eq!(
            readable_path(PathBuf::from(r"\\?\C:\Users\Example\Downloads")),
            PathBuf::from(r"C:\Users\Example\Downloads")
        );
        assert_eq!(
            readable_path(PathBuf::from(r"\\?\UNC\server\share\Downloads")),
            PathBuf::from(r"\\server\share\Downloads")
        );
    }
}

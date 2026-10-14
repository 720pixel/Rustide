use std::{
    ffi::{OsStr, OsString},
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, ensure};
use windows::{
    Win32::{
        Foundation::{ERROR_CANCELLED, HWND},
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            CoTaskMemFree, CoUninitialize,
        },
        UI::Shell::{
            Common::COMDLG_FILTERSPEC, FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST,
            FOS_FORCEFILESYSTEM, FOS_OVERWRITEPROMPT, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS,
            FileOpenDialog, FileSaveDialog, IFileDialog, IFileOpenDialog, IFileSaveDialog,
            IShellItem, SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
        },
    },
    core::{HRESULT, Interface, PCWSTR, PWSTR, w},
};

use super::picker::PickMode;

pub fn pick(
    ctx: &egui::Context,
    mode: PickMode,
    current: &str,
    torrent_only: bool,
) -> Result<Option<PathBuf>> {
    // The shell dialog runs modally on the window's STA thread.
    let _apartment = Apartment::new()?;
    let dialog: IFileDialog = unsafe {
        if mode == PickMode::Save {
            let save: IFileSaveDialog =
                CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER)?;
            save.cast()?
        } else {
            let open: IFileOpenDialog =
                CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
            open.cast()?
        }
    };
    unsafe {
        let mut options = dialog.GetOptions()? | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST;
        options &= !FOS_ALLOWMULTISELECT;
        if mode == PickMode::Folder {
            options |= FOS_PICKFOLDERS | FOS_FILEMUSTEXIST;
        } else if mode == PickMode::File {
            options |= FOS_FILEMUSTEXIST;
        } else {
            // Saving is confirmed by the editor or create action.
            options &= !FOS_OVERWRITEPROMPT;
        }
        dialog.SetOptions(options)?;
        dialog.SetTitle(match mode {
            PickMode::File if torrent_only => w!("Open torrent"),
            PickMode::File => w!("Choose a file"),
            PickMode::Folder => w!("Choose a folder"),
            PickMode::Save => w!("Save torrent as"),
        })?;
        if mode != PickMode::Folder {
            if torrent_only || mode == PickMode::Save {
                dialog.SetFileTypes(&[
                    COMDLG_FILTERSPEC {
                        pszName: w!("Torrent files (*.torrent)"),
                        pszSpec: w!("*.torrent"),
                    },
                    COMDLG_FILTERSPEC {
                        pszName: w!("All files (*.*)"),
                        pszSpec: w!("*.*"),
                    },
                ])?;
                dialog.SetDefaultExtension(w!("torrent"))?;
            } else {
                dialog.SetFileTypes(&[COMDLG_FILTERSPEC {
                    pszName: w!("All files (*.*)"),
                    pszSpec: w!("*.*"),
                }])?;
            }
            dialog.SetFileTypeIndex(1)?;
        }
    }
    let path = if current.trim().is_empty() {
        None
    } else {
        Some(super::model::expand_path(current)?)
    };
    if let Some(directory) = path.as_deref().and_then(start_directory) {
        let directory =
            std::path::absolute(directory).context("Cannot resolve the starting folder")?;
        let wide = wide(directory.as_os_str())?;
        let item: IShellItem = unsafe {
            SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None)
                .context("Cannot open the starting folder")?
        };
        unsafe { dialog.SetFolder(&item)? };
    }
    if mode == PickMode::Save {
        let filename = path
            .as_deref()
            .filter(|path| !path.is_dir())
            .and_then(Path::file_name)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| OsStr::new("untitled.torrent"));
        let filename = wide(filename)?;
        unsafe { dialog.SetFileName(PCWSTR(filename.as_ptr()))? };
    }
    let owner = ctx
        .data(|data| data.get_temp::<usize>(egui::Id::new("native-window")))
        .unwrap_or_default();
    match unsafe { dialog.Show(Some(HWND(owner as *mut _))) } {
        Ok(()) => {}
        Err(error) if error.code() == HRESULT::from_win32(ERROR_CANCELLED.0) => return Ok(None),
        Err(error) => return Err(error).context("Cannot open the file dialog"),
    }
    let item = unsafe { dialog.GetResult()? };
    let filename = TaskMemory(unsafe { item.GetDisplayName(SIGDN_FILESYSPATH)? });
    let result = PathBuf::from(OsString::from_wide(unsafe { filename.0.as_wide() }));
    match mode {
        PickMode::File => ensure!(result.is_file(), "Select an existing file"),
        PickMode::Folder => ensure!(result.is_dir(), "Select an existing folder"),
        PickMode::Save => {
            ensure!(!result.is_dir(), "The output path names a folder");
            ensure!(
                result.parent().is_some_and(Path::is_dir),
                "The output folder does not exist"
            );
        }
    }
    Ok(Some(result))
}

fn start_directory(path: &Path) -> Option<PathBuf> {
    if path.is_dir() {
        Some(path.to_owned())
    } else {
        path.ancestors().skip(1).find_map(|parent| {
            if parent.as_os_str().is_empty() {
                std::env::current_dir().ok()
            } else {
                parent.is_dir().then(|| parent.to_owned())
            }
        })
    }
}

fn wide(text: &OsStr) -> Result<Vec<u16>> {
    let mut text: Vec<u16> = text.encode_wide().collect();
    ensure!(!text.contains(&0), "path contains NUL");
    text.push(0);
    Ok(text)
}

struct Apartment;

impl Apartment {
    fn new() -> Result<Self> {
        // S_FALSE also acquires a COM initialization that needs balancing.
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
        Ok(Self)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

struct TaskMemory(PWSTR);

impl Drop for TaskMemory {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0.0.cast())) };
    }
}

#[cfg(test)]
mod tests {
    use super::start_directory;

    #[test]
    fn existing_file_starts_in_its_parent_folder() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("résumé.torrent");
        std::fs::write(&file, b"torrent").unwrap();
        assert_eq!(start_directory(&file).as_deref(), Some(directory.path()));
        assert_eq!(
            start_directory(directory.path()).as_deref(),
            Some(directory.path())
        );
    }

    #[test]
    fn removed_remembered_folder_falls_back_to_existing_parent() {
        let directory = tempfile::tempdir().unwrap();
        let removed = directory.path().join("previous");
        std::fs::create_dir(&removed).unwrap();
        std::fs::remove_dir(&removed).unwrap();
        assert_eq!(
            start_directory(&removed.join("nested").join("output.torrent")).as_deref(),
            Some(directory.path())
        );
    }
}

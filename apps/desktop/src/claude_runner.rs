//! The tray's Claude runner: `broadcast runner claude` inside the app, against the board this
//! app serves, in the folder the user chose. Whether it runs and where persist across launches.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::async_runtime::JoinHandle;
use tauri::menu::{CheckMenuItem, MenuItem};
use tauri::{AppHandle, Wry};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use url::Url;

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Settings {
    pub enabled: bool,
    pub cwd: Option<PathBuf>,
}

pub struct ClaudeRunner {
    path: PathBuf,
    settings: Mutex<Settings>,
    /// The board the app serves now; its token changes each time the service starts.
    board: Mutex<Option<Url>>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl ClaudeRunner {
    pub fn load(data_directory: &Path) -> Self {
        let path = data_directory.join("claude-runner.json");
        let settings = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self {
            path,
            settings: Mutex::new(settings),
            board: Mutex::new(None),
            task: Mutex::new(None),
        }
    }

    pub fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    /// Saves the settings and starts or stops the runner to match them.
    pub fn update(&self, settings: Settings) -> Result<(), String> {
        let text = serde_json::to_string_pretty(&settings).map_err(|error| error.to_string())?;
        fs::write(&self.path, text).map_err(|error| error.to_string())?;
        *self.settings.lock().unwrap() = settings;
        self.restart();
        Ok(())
    }

    /// The service now serves `board`; an enabled runner restarts against it.
    pub fn attach(&self, board: Url) {
        *self.board.lock().unwrap() = Some(board);
        self.restart();
    }

    /// The service stopped; the runner stops with it.
    pub fn detach(&self) {
        *self.board.lock().unwrap() = None;
        self.restart();
    }

    /// Aborting the task drops the runner future, which also kills a running `claude`.
    fn restart(&self) {
        let mut task = self.task.lock().unwrap();
        if let Some(running) = task.take() {
            running.abort();
        }
        let Settings { enabled, cwd } = self.settings();
        let board = self.board.lock().unwrap().clone();
        let (true, Some(cwd), Some(url)) = (enabled, cwd, board) else {
            return;
        };
        let options = broadcast_agents::claude::Options {
            url,
            cwd,
            permission_mode: "acceptEdits".into(),
            interval: Duration::from_secs(5),
        };
        *task = Some(tauri::async_runtime::spawn(async move {
            if let Err(error) = broadcast_agents::claude::run(options).await {
                println!("[claude-runner] stopped: {error:#}");
            }
        }));
    }
}

/// The tray's runner toggle and folder item, kept in step with the saved settings.
#[derive(Clone)]
pub struct Tray {
    runner: Arc<ClaudeRunner>,
    pub toggle: CheckMenuItem<Wry>,
    pub folder: MenuItem<Wry>,
}

impl Tray {
    pub fn new(app: &tauri::App, runner: Arc<ClaudeRunner>) -> tauri::Result<Self> {
        let settings = runner.settings();
        let toggle = CheckMenuItem::with_id(
            app,
            "claude-runner",
            "Claude runner",
            true,
            settings.enabled,
            None::<&str>,
        )?;
        let folder = MenuItem::with_id(
            app,
            "claude-runner-folder",
            folder_label(&settings),
            true,
            None::<&str>,
        )?;
        Ok(Self {
            runner,
            toggle,
            folder,
        })
    }

    /// Turning the runner on before a folder is chosen asks for one first.
    pub fn toggle(&self, app: &AppHandle) {
        let mut settings = self.runner.settings();
        if settings.cwd.is_none() && !settings.enabled {
            self.choose_folder(app, true);
            return;
        }
        settings.enabled = !settings.enabled;
        self.apply(app, settings);
    }

    pub fn choose_folder(&self, app: &AppHandle, enable: bool) {
        let tray = self.clone();
        let handle = app.clone();
        let mut dialog = app
            .dialog()
            .file()
            .set_title("选择 Claude runner 的工作目录");
        if let Some(cwd) = self.runner.settings().cwd {
            dialog = dialog.set_directory(cwd);
        }
        dialog.pick_folder(move |folder| {
            let mut settings = tray.runner.settings();
            if let Some(path) = folder.and_then(|folder| folder.into_path().ok()) {
                settings.cwd = Some(path);
                settings.enabled |= enable;
            }
            tray.apply(&handle, settings);
        });
    }

    fn apply(&self, app: &AppHandle, settings: Settings) {
        if let Err(error) = self.runner.update(settings) {
            app.dialog()
                .message(error)
                .kind(MessageDialogKind::Error)
                .title("Claude runner 设置失败")
                .show(|_| {});
        }
        let settings = self.runner.settings();
        let _ = self.toggle.set_checked(settings.enabled);
        let _ = self.folder.set_text(folder_label(&settings));
    }
}

fn folder_label(settings: &Settings) -> String {
    match &settings.cwd {
        Some(cwd) => format!("Claude 工作目录：{}", cwd.display()),
        None => "选择 Claude 工作目录…".into(),
    }
}

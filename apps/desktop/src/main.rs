#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(target_os = "macos")]
use dispatch2::{run_on_main, MainThreadBound};
use futures_util::{future::{BoxFuture, Shared}, FutureExt};
#[cfg(target_os = "macos")]
use objc2::{
    define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, NSObjectProtocol},
    sel, DefinedClass, MainThreadMarker, MainThreadOnly,
};
#[cfg(target_os = "macos")]
use objc2_app_kit::{
    NSAlert, NSApplication, NSButton, NSProgressIndicator, NSProgressIndicatorStyle,
    NSRunningApplication,
};
#[cfg(target_os = "macos")]
use objc2_foundation::{NSObject, NSSize, NSString};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(target_os = "macos")]
use std::cell::RefCell;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::os::fd::AsRawFd;
#[cfg(target_os = "macos")]
use std::os::unix::process::CommandExt;
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::Write,
    net::{Ipv4Addr, TcpListener},
    path::{Path, PathBuf},
    process::Command as StdCommand,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::time::Instant;
#[cfg(target_os = "windows")]
use std::os::windows::fs::OpenOptionsExt;
#[cfg(target_os = "macos")]
use tauri::ActivationPolicy;
use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem},
    tray::TrayIconBuilder,
    AppHandle, Emitter, Manager,
};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_updater::{Update, UpdaterExt};
use uuid::Uuid;
#[cfg(target_os = "windows")]
use windows::{
    core::PWSTR,
    Win32::{
        Foundation::{CloseHandle, ERROR_SUCCESS, FILETIME, WAIT_OBJECT_0},
        System::{
            RestartManager::{
                RmEndSession, RmRegisterResources, RmShutdown, RmStartSession, CCH_RM_SESSION_KEY,
                RM_UNIQUE_PROCESS,
            },
            Threading::{
                GetProcessTimes, OpenProcess, WaitForSingleObject,
                PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
            },
        },
    },
};

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
const LAUNCHER_STOP_TIMEOUT: Duration = Duration::from_secs(36);
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(30 * 60);
const BETA_UPDATER_ENDPOINT: &str =
    "https://raw.githubusercontent.com/iinoshirozheng/broadcast/beta-updater/latest.json";
// Unique whole-directory snapshots shipped from app-v0.2.0 through v1.1.2.
const KNOWN_BROADCAST_SKILL_DIGESTS: [&str; 6] = [
    "eeaaa5d71a2c47688bf62a5eb9f45e9138fe49eb636a46cfd6af8a0f8853e2e0",
    "c4ce3257bbf3efed1bb4d2d9f26436be8ba835d4ab4adf6fed38f5abbedafa59",
    "6f1b1bb3a731aa154018c97b0779442f6c461cb5dd3ea91ca49da4bb3b8a8ea0",
    "8ab19649d29cad0a39b0ab202b909bf03de07e37837b05cd5c3df5a4da0119f8",
    "27131c82ac63c2884c1fcb7dd22a4e1c75975c7d79eb3fa3483a7949dd5f284d",
    "ae74aec793decf6d9013c36f4b53e01723796a45567b77e9e9f22b4a168d3fbe",
];
const BROADCAST_PREFERRED_PORT: u16 = 47823;
#[cfg(target_os = "macos")]
const MACOS_BUNDLE_MIGRATION_SOURCE_ENV: &str =
    "BROADCAST_MACOS_BUNDLE_MIGRATION_SOURCE";
#[cfg(target_os = "macos")]
const MACOS_BUNDLE_MIGRATION_BETA_AUTOSTART_ENV: &str =
    "BROADCAST_MACOS_BUNDLE_MIGRATION_BETA_AUTOSTART";

fn release_version() -> &'static str {
    option_env!("BROADCAST_RELEASE_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

fn is_beta_release() -> bool {
    release_version().contains("-beta.")
}

#[cfg(target_os = "macos")]
struct MacosBundleMigration {
    source_executable: PathBuf,
    beta_autostart_was_enabled: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LauncherSnapshot {
    phase: String,
    message: String,
    update_message: String,
    update_available: bool,
    version: String,
    app_path: Option<String>,
}

#[derive(Deserialize)]
struct LauncherRuntimeDescriptor {
    url: String,
}

/// The embedded server and the Codex launcher that serves it, started together.
struct Service {
    generation: u64,
    launcher: broadcast_codex::Launcher,
    /// Receives once both have stopped.
    stopped: std::sync::mpsc::Receiver<()>,
}

struct LauncherState {
    service: Mutex<Option<Service>>,
    snapshot: Mutex<LauncherSnapshot>,
    status_menu: Mutex<Option<MenuItem<tauri::Wry>>>,
    intentional_stop: AtomicBool,
    update_flow_in_progress: AtomicBool,
    update_in_progress: AtomicBool,
    pending_update: tauri::async_runtime::Mutex<Option<PendingUpdate>>,
    generation: AtomicU64,
    lifecycle: Mutex<()>,
    broadcast_listener: Mutex<Option<TcpListener>>,
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    codex_port: Mutex<Option<u16>>,
    _instance_lock: File,
    data_directory: PathBuf,
    log_path: PathBuf,
}

#[derive(Clone)]
struct PendingUpdate {
    update: Update,
    download: Shared<BoxFuture<'static, Result<Arc<Vec<u8>>, String>>>,
    dialog: Arc<Mutex<Option<UpdateDialog>>>,
}

#[cfg(target_os = "macos")]
struct UpdateDialogTargetIvars {
    response: RefCell<Option<std::sync::mpsc::Sender<bool>>>,
}

#[cfg(target_os = "macos")]
define_class!(
    #[unsafe(super = NSObject)]
    #[name = "BroadcastUpdateDialogTarget"]
    #[thread_kind = MainThreadOnly]
    #[ivars = UpdateDialogTargetIvars]
    struct UpdateDialogTarget;

    unsafe impl NSObjectProtocol for UpdateDialogTarget {}

    impl UpdateDialogTarget {
        #[unsafe(method(acceptUpdate:))]
        fn accept_update(&self, _sender: &AnyObject) {
            self.respond(true);
        }

        #[unsafe(method(deferUpdate:))]
        fn defer_update(&self, _sender: &AnyObject) {
            self.respond(false);
        }
    }
);

#[cfg(target_os = "macos")]
impl UpdateDialogTarget {
    fn new(mtm: MainThreadMarker, response: std::sync::mpsc::Sender<bool>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(UpdateDialogTargetIvars {
            response: RefCell::new(Some(response)),
        });
        unsafe { msg_send![super(this), init] }
    }

    fn respond(&self, accepted: bool) {
        if let Some(response) = self.ivars().response.borrow_mut().take() {
            let _ = response.send(accepted);
        }
    }
}

#[cfg(target_os = "macos")]
struct NativeUpdateDialog {
    alert: Retained<NSAlert>,
    progress_indicator: Retained<NSProgressIndicator>,
    install_button: Retained<NSButton>,
    defer_button: Retained<NSButton>,
    _target: Retained<UpdateDialogTarget>,
}

#[cfg(target_os = "macos")]
#[derive(Clone)]
struct UpdateDialog {
    native: Arc<MainThreadBound<NativeUpdateDialog>>,
}

#[cfg(target_os = "macos")]
impl UpdateDialog {
    fn prompt(_app: &AppHandle, version: &str) -> Option<Self> {
        let message = format!(
            "发现新版本 Broadcast {version}。是否现在更新并重启？"
        );
        let (response, result) = std::sync::mpsc::channel();
        let dialog = run_on_main(move |mtm| {
            let alert = NSAlert::new(mtm);
            let target = UpdateDialogTarget::new(mtm, response);
            let progress_indicator = NSProgressIndicator::new(mtm);
            progress_indicator.setStyle(NSProgressIndicatorStyle::Bar);
            progress_indicator.setMinValue(0.0);
            progress_indicator.setMaxValue(100.0);
            progress_indicator.setFrameSize(NSSize::new(280.0, 20.0));
            progress_indicator.sizeToFit();
            progress_indicator.setDisplayedWhenStopped(true);
            alert.setMessageText(&NSString::from_str("Broadcast 更新"));
            alert.setInformativeText(&NSString::from_str(&message));
            let install_button = alert.addButtonWithTitle(&NSString::from_str("立即更新"));
            let defer_button = alert.addButtonWithTitle(&NSString::from_str("稍后"));
            unsafe {
                install_button.setTarget(Some(&target));
                install_button.setAction(Some(sel!(acceptUpdate:)));
                defer_button.setTarget(Some(&target));
                defer_button.setAction(Some(sel!(deferUpdate:)));
            }
            alert.layout();
            let window = alert.window();
            window.center();
            NSApplication::sharedApplication(mtm).activate();
            window.makeKeyAndOrderFront(None);
            window.orderFrontRegardless();
            Self {
                native: Arc::new(MainThreadBound::new(
                    NativeUpdateDialog {
                        alert,
                        progress_indicator,
                        install_button,
                        defer_button,
                        _target: target,
                    },
                    mtm,
                )),
            }
        });
        if result.recv().unwrap() {
            Some(dialog)
        } else {
            dialog.close();
            None
        }
    }

    fn show_installing(&self, message: &str) {
        let native = Arc::clone(&self.native);
        let message = message.to_owned();
        run_on_main(move |mtm| {
            let native = native.get(mtm);
            native
                .alert
                .setInformativeText(&NSString::from_str(&message));
            native.progress_indicator.setIndeterminate(false);
            native.progress_indicator.setDoubleValue(100.0);
            native
                .alert
                .setAccessoryView(Some(&native.progress_indicator));
            native.install_button.setHidden(true);
            native.defer_button.setEnabled(false);
            native.defer_button.setHidden(true);
            native.alert.layout();
            native.progress_indicator.setNeedsDisplay(true);
            native.progress_indicator.displayIfNeeded();
        });
    }

    fn set_progress(&self, message: &str, progress: Option<u64>, cancellable: bool) {
        let native = Arc::clone(&self.native);
        let message = message.to_owned();
        run_on_main(move |mtm| {
            let native = native.get(mtm);
            native
                .alert
                .setInformativeText(&NSString::from_str(&message));
            native.progress_indicator.setIndeterminate(progress.is_none());
            if let Some(progress) = progress {
                unsafe {
                    native.progress_indicator.stopAnimation(None);
                }
                native.progress_indicator.setDoubleValue(progress as f64);
            } else {
                unsafe {
                    native.progress_indicator.startAnimation(None);
                }
            }
            native.alert.setAccessoryView(Some(&native.progress_indicator));
            native.install_button.setHidden(true);
            if !cancellable {
                native.defer_button.setEnabled(false);
                native.defer_button.setHidden(true);
            }
            native.alert.layout();
            native.progress_indicator.setNeedsDisplay(true);
            native.progress_indicator.displayIfNeeded();
        });
    }

    fn close(&self) {
        let native = Arc::clone(&self.native);
        run_on_main(move |mtm| {
            let native = native.get(mtm);
            native.alert.window().close();
        });
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone)]
struct UpdateDialog;

#[cfg(target_os = "linux")]
impl UpdateDialog {
    fn prompt(app: &AppHandle, version: &str) -> Option<Self> {
        app.dialog()
            .message(format!(
                "发现新版本 Broadcast {version}。是否现在更新并重启？"
            ))
            .title("Broadcast 更新")
            .kind(MessageDialogKind::Info)
            .buttons(MessageDialogButtons::OkCancelCustom(
                "立即更新".into(),
                "稍后".into(),
            ))
            .blocking_show()
            .then_some(Self)
    }

    fn show_installing(&self, _message: &str) {}

    fn set_progress(&self, _message: &str, _progress: Option<u64>, _cancellable: bool) {}

    fn close(&self) {}
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
#[derive(Clone)]
struct UpdateDialog;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
impl UpdateDialog {
    fn prompt(_app: &AppHandle, _version: &str) -> Option<Self> {
        None
    }

    fn show_installing(&self, _message: &str) {}

    fn set_progress(&self, _message: &str, _progress: Option<u64>, _cancellable: bool) {}

    fn close(&self) {}
}

impl LauncherState {
    fn new(
        data_directory: PathBuf,
        log_directory: PathBuf,
        version: String,
        instance_lock: File,
    ) -> Self {
        Self {
            service: Mutex::new(None),
            snapshot: Mutex::new(LauncherSnapshot {
                phase: "starting".into(),
                message: "正在启动任务面板…".into(),
                update_message: "启动后将自动检查更新。".into(),
                update_available: false,
                version,
                app_path: None,
            }),
            status_menu: Mutex::new(None),
            intentional_stop: AtomicBool::new(false),
            update_flow_in_progress: AtomicBool::new(false),
            update_in_progress: AtomicBool::new(false),
            pending_update: tauri::async_runtime::Mutex::new(None),
            generation: AtomicU64::new(0),
            lifecycle: Mutex::new(()),
            broadcast_listener: Mutex::new(None),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            codex_port: Mutex::new(None),
            _instance_lock: instance_lock,
            data_directory,
            log_path: log_directory.join("broadcast-launcher.log"),
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn acquire_instance_lock(path: &Path) -> Result<Option<File>, std::io::Error> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        Ok(Some(file))
    } else {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            Ok(None)
        } else {
            Err(error)
        }
    }
}

#[cfg(target_os = "macos")]
fn macos_app_path_from_executable(executable: &Path) -> Option<PathBuf> {
    let macos_directory = executable.parent()?;
    if macos_directory.file_name()? != std::ffi::OsStr::new("MacOS") {
        return None;
    }
    let contents_directory = macos_directory.parent()?;
    if contents_directory.file_name()? != std::ffi::OsStr::new("Contents") {
        return None;
    }
    let app_path = contents_directory.parent()?;
    (app_path.extension()? == std::ffi::OsStr::new("app")).then(|| app_path.to_path_buf())
}

#[cfg(target_os = "macos")]
fn append_macos_startup_log(line: &str) {
    let Some(home_directory) = std::env::var_os("HOME").map(PathBuf::from) else {
        return;
    };
    let log_directory = home_directory.join("Library/Logs/Broadcast");
    if fs::create_dir_all(&log_directory).is_err() {
        return;
    }
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_directory.join("broadcast-launcher.log"))
    {
        let _ = writeln!(file, "{line}");
    }
}

#[cfg(target_os = "macos")]
fn wait_for_macos_bundle_migration_lock() -> Result<File, String> {
    let home_directory = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is unavailable".to_string())?;
    let data_directory = home_directory.join("Library/Application Support/Broadcast");
    fs::create_dir_all(&data_directory).map_err(|error| {
        format!(
            "无法创建应用数据目录 {}：{error}",
            data_directory.display()
        )
    })?;
    let lock_path = data_directory.join("launcher.lock");
    let deadline = Instant::now() + LAUNCHER_STOP_TIMEOUT;
    loop {
        match acquire_instance_lock(&lock_path) {
            Ok(Some(file)) => return Ok(file),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(100)),
            Ok(None) => {
                return Err(format!(
                    "等待现有 App 退出超时，无法迁移 {}",
                    lock_path.display()
                ));
            }
            Err(error) => {
                return Err(format!(
                    "无法锁定 App 迁移路径 {}：{error}",
                    lock_path.display()
                ));
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn rename_macos_app_bundle(source: &Path, destination: &Path) -> Result<(), String> {
    match fs::rename(source, destination) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {}
        Err(error) => {
            return Err(format!(
                "无法将 {} 改名为 {}：{error}",
                source.display(),
                destination.display()
            ));
        }
    }

    let script = r#"on run argv
set sourcePath to item 1 of argv
set destinationPath to item 2 of argv
do shell script ("/bin/mv " & quoted form of sourcePath & " " & quoted form of destinationPath) with administrator privileges
end run"#;
    let output = StdCommand::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .arg(source)
        .arg(destination)
        .output()
        .map_err(|error| format!("无法请求 App 改名授权：{error}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if detail.is_empty() {
            "App 改名授权未完成".into()
        } else {
            format!("App 改名授权未完成：{detail}")
        });
    }
    if source.exists() || !destination.is_dir() {
        return Err(format!(
            "App 改名后路径状态不正确：{} -> {}",
            source.display(),
            destination.display()
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn take_macos_bundle_migration_marker() -> Result<Option<MacosBundleMigration>, String> {
    let source_executable =
        std::env::var_os(MACOS_BUNDLE_MIGRATION_SOURCE_ENV).map(PathBuf::from);
    let beta_autostart_marker =
        std::env::var_os(MACOS_BUNDLE_MIGRATION_BETA_AUTOSTART_ENV);
    // SAFETY: `main` calls this before Tauri or tokio start any thread.
    unsafe {
        std::env::remove_var(MACOS_BUNDLE_MIGRATION_SOURCE_ENV);
        std::env::remove_var(MACOS_BUNDLE_MIGRATION_BETA_AUTOSTART_ENV);
    }
    let Some(source_executable) = source_executable else {
        return Ok(None);
    };
    let beta_autostart_was_enabled =
        beta_autostart_marker.as_deref() == Some(std::ffi::OsStr::new("1"));
    if !is_beta_release() {
        return Err("稳定版不能恢复 Beta App bundle migration marker".into());
    }

    let current_executable =
        std::env::current_exe().map_err(|error| format!("无法定位当前可执行文件：{error}"))?;
    let current_executable = fs::canonicalize(&current_executable)
        .map_err(|error| format!("无法解析当前可执行文件路径：{error}"))?;
    let current_app = macos_app_path_from_executable(&current_executable)
        .ok_or_else(|| "当前可执行文件不在 macOS App bundle 内".to_string())?;
    if current_app.file_name() != Some(std::ffi::OsStr::new("Broadcast Beta.app")) {
        return Err(format!(
            "macOS App bundle migration marker 只能由改名后的 Beta App 恢复：{}",
            current_app.display()
        ));
    }
    let relative_executable = current_executable
        .strip_prefix(&current_app)
        .map_err(|error| format!("无法解析 Beta App 可执行文件相对路径：{error}"))?;
    let expected_source_executable = current_app
        .parent()
        .ok_or_else(|| format!("无法定位 App 上级目录：{}", current_app.display()))?
        .join("Broadcast.app")
        .join(relative_executable);
    if source_executable != expected_source_executable {
        return Err(format!(
            "macOS App bundle migration source marker 不匹配：{} != {}",
            source_executable.display(),
            expected_source_executable.display()
        ));
    }

    Ok(Some(MacosBundleMigration {
        source_executable,
        beta_autostart_was_enabled,
    }))
}

#[cfg(target_os = "macos")]
fn migrate_macos_beta_app_bundle_name() -> Result<Option<MacosBundleMigration>, String> {
    if let Some(migration) = take_macos_bundle_migration_marker()? {
        return Ok(Some(migration));
    }
    if !is_beta_release() {
        return Ok(None);
    }

    let current_executable =
        std::env::current_exe().map_err(|error| format!("无法定位当前可执行文件：{error}"))?;
    let current_executable = fs::canonicalize(&current_executable)
        .map_err(|error| format!("无法解析当前可执行文件路径：{error}"))?;
    let Some(current_app) = macos_app_path_from_executable(&current_executable) else {
        return Ok(None);
    };
    if current_app.file_name() == Some(std::ffi::OsStr::new("Broadcast Beta.app")) {
        return Ok(None);
    }
    if current_app.file_name() != Some(std::ffi::OsStr::new("Broadcast.app")) {
        return Err(format!(
            "Beta App 当前路径名称不受支持：{}",
            current_app.display()
        ));
    }
    let destination_app = current_app
        .parent()
        .ok_or_else(|| format!("无法定位 App 上级目录：{}", current_app.display()))?
        .join("Broadcast Beta.app");
    let executable_name = current_executable
        .file_name()
        .ok_or_else(|| format!("无法定位 App 可执行文件名：{}", current_executable.display()))?
        .to_owned();
    let destination_executable = destination_app
        .join("Contents/MacOS")
        .join(executable_name);
    let instance_lock = wait_for_macos_bundle_migration_lock()?;
    let fd_flags = unsafe { libc::fcntl(instance_lock.as_raw_fd(), libc::F_GETFD) };
    if fd_flags < 0
        || unsafe {
            libc::fcntl(
                instance_lock.as_raw_fd(),
                libc::F_SETFD,
                fd_flags | libc::FD_CLOEXEC,
            )
        } < 0
    {
        return Err(format!(
            "无法设置 App 迁移锁：{}",
            std::io::Error::last_os_error()
        ));
    }

    if !current_app.is_dir() {
        return Err(format!("当前 App 路径不存在：{}", current_app.display()));
    }
    if destination_app.exists() {
        return Err(format!(
            "目标 App 路径已存在，未覆盖：{}",
            destination_app.display()
        ));
    }
    let home_directory = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is unavailable".to_string())?;
    let beta_autostart_was_enabled = home_directory
        .join("Library/LaunchAgents/Broadcast Beta.plist")
        .is_file();
    let beta_autostart_marker = if beta_autostart_was_enabled { "1" } else { "0" };

    rename_macos_app_bundle(&current_app, &destination_app)?;
    append_macos_startup_log(&format!(
        "Migrated macOS App bundle {} -> {}",
        current_app.display(),
        destination_app.display()
    ));

    let mut command = StdCommand::new(&destination_executable);
    command
        .args(std::env::args_os().skip(1))
        .env(MACOS_BUNDLE_MIGRATION_SOURCE_ENV, &current_executable)
        .env(
            MACOS_BUNDLE_MIGRATION_BETA_AUTOSTART_ENV,
            beta_autostart_marker,
        );
    let exec_error = command.exec();

    match rename_macos_app_bundle(&destination_app, &current_app) {
        Ok(()) => Err(format!(
            "无法从改名后的 App 重启，已恢复原路径：{exec_error}"
        )),
        Err(rollback_error) => {
            append_macos_startup_log(&format!(
                "Failed to restart renamed macOS App: {exec_error}; rollback failed: {rollback_error}"
            ));
            std::process::exit(1);
        }
    }
}

#[cfg(target_os = "windows")]
fn acquire_instance_lock(path: &Path) -> Result<Option<File>, std::io::Error> {
    match OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .share_mode(0)
        .open(path)
    {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.raw_os_error() == Some(32) => Ok(None),
        Err(error) => Err(error),
    }
}

fn copy_directory(source: &Path, destination: &Path) -> Result<(), std::io::Error> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let destination = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_directory(&entry.path(), &destination)?;
        } else {
            fs::copy(entry.path(), destination)?;
        }
    }
    Ok(())
}

fn collect_skill_files(
    root: &Path,
    directory: &Path,
    files: &mut Vec<PathBuf>,
) -> Result<bool, std::io::Error> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Ok(false);
        }
        if file_type.is_dir() {
            let file_count = files.len();
            if !collect_skill_files(root, &entry.path(), files)? {
                return Ok(false);
            }
            if files.len() == file_count {
                return Ok(false);
            }
        } else if file_type.is_file() {
            files.push(entry.path().strip_prefix(root).unwrap().to_path_buf());
        } else {
            return Ok(false);
        }
    }
    Ok(true)
}

fn skill_directory_digest(directory: &Path) -> Result<Option<String>, std::io::Error> {
    let mut files = Vec::new();
    if !collect_skill_files(directory, directory, &mut files)? {
        return Ok(None);
    }
    files.sort();
    let mut digest = Sha256::new();
    for relative_path in files {
        let contents = fs::read(directory.join(&relative_path))?;
        digest.update(relative_path.to_string_lossy().replace('\\', "/"));
        digest.update([0]);
        digest.update((contents.len() as u64).to_le_bytes());
        digest.update(contents);
    }
    Ok(Some(format!("{:x}", digest.finalize())))
}

fn reconcile_legacy_skill(
    home_directory: &Path,
    bundled_skill: &Path,
) -> Result<Option<(PathBuf, PathBuf)>, std::io::Error> {
    let legacy_skill = home_directory.join(".codex/skills/manage-broadcast");
    let metadata = match fs::symlink_metadata(&legacy_skill) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };

    if metadata.file_type().is_symlink() {
        fs::remove_dir_all(legacy_skill)?;
        return Ok(None);
    }

    if metadata.is_dir() {
        let legacy_digest = skill_directory_digest(&legacy_skill)?;
        let bundled_digest = skill_directory_digest(bundled_skill)?;
        let known_copy = legacy_digest.as_ref().is_some_and(|digest| {
            bundled_digest.as_ref() == Some(digest)
                || KNOWN_BROADCAST_SKILL_DIGESTS.contains(&digest.as_str())
        });
        if known_copy {
            fs::remove_dir_all(legacy_skill)?;
            return Ok(None);
        }
    }

    let backup_path = home_directory
        .join(".codex/broadcast-skill-backups")
        .join(format!("manage-broadcast-{}", Uuid::new_v4()));
    Ok(Some((legacy_skill, backup_path)))
}

fn resolve_legacy_skill_conflict(
    app: &AppHandle,
    legacy_skill: &Path,
    backup_path: &Path,
) -> Result<bool, std::io::Error> {
    let proceed = app
        .dialog()
        .message(format!(
            "检测到旧位置中的 manage-broadcast Skill 与当前 App 内置版本不同，可能包含你的修改。\n\n为避免 Codex 同时发现两个版本，Broadcast 会把旧副本完整保留到：\n\n{}\n\n选择退出不会改动旧副本，也不会启动 Codex。",
            backup_path.display()
        ))
        .title("Broadcast Skill 冲突")
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom(
            "保留备份并继续".into(),
            "退出".into(),
        ))
        .blocking_show();
    if !proceed {
        return Ok(false);
    }

    fs::create_dir_all(backup_path.parent().unwrap())?;
    fs::rename(legacy_skill, backup_path)?;
    Ok(true)
}

fn loopback_listener() -> Result<TcpListener, String> {
    TcpListener::bind(("127.0.0.1", 0)).map_err(|error| error.to_string())
}

fn broadcast_loopback_listener() -> Result<TcpListener, String> {
    TcpListener::bind(("127.0.0.1", BROADCAST_PREFERRED_PORT))
        .or_else(|_| TcpListener::bind(("127.0.0.1", 0)))
        .map_err(|error| error.to_string())
}

/// One loopback listener for the app's lifetime, so the board keeps its port across restarts.
fn broadcast_listener(state: &LauncherState) -> Result<TcpListener, String> {
    let mut listener = state.broadcast_listener.lock().unwrap();
    if listener.is_none() {
        *listener = Some(broadcast_loopback_listener()?);
    }
    listener
        .as_ref()
        .unwrap()
        .try_clone()
        .map_err(|error| error.to_string())
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn codex_port(state: &LauncherState) -> Result<u16, String> {
    let mut port = state.codex_port.lock().unwrap();
    if let Some(port) = *port {
        return Ok(port);
    }
    // Development builds can attach to a stand-in app already listening on a known port.
    #[cfg(debug_assertions)]
    if let Some(selected) = std::env::var("BROADCAST_CODEX_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
    {
        *port = Some(selected);
        return Ok(selected);
    }
    let listener = loopback_listener()?;
    let selected = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    *port = Some(selected);
    Ok(selected)
}

fn update_snapshot(
    app: &AppHandle,
    state: &Arc<LauncherState>,
    update: impl FnOnce(&mut LauncherSnapshot),
) -> LauncherSnapshot {
    let snapshot = {
        let mut snapshot = state.snapshot.lock().unwrap();
        update(&mut snapshot);
        snapshot.clone()
    };
    let status_menu = state.status_menu.lock().unwrap().clone();
    if let Some(status_menu) = status_menu {
        let status_state = Arc::clone(state);
        let _ = app.run_on_main_thread(move || {
            let status = {
                let snapshot = status_state.snapshot.lock().unwrap();
                match snapshot.phase.as_str() {
                    "running" => "运行状态：正常",
                    "error" => "运行状态：异常",
                    _ => "运行状态：启动中",
                }
            };
            let _ = status_menu.set_text(status);
        });
    }
    let _ = app.emit("launcher-status", snapshot.clone());
    snapshot
}

fn append_log(state: &LauncherState, line: &str) {
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&state.log_path)
    {
        let _ = writeln!(file, "{line}");
    }
}

fn show_error_dialog(app: &AppHandle, title: &str, message: &str) {
    app.dialog()
        .message(message)
        .title(title)
        .kind(MessageDialogKind::Error)
        .buttons(MessageDialogButtons::OkCustom("关闭".into()))
        .blocking_show();
}

#[cfg(target_os = "macos")]
fn macos_launch_agent_executable(entry: &Path) -> Option<PathBuf> {
    if !entry.is_file() {
        return None;
    }

    let output = StdCommand::new("/usr/bin/plutil")
        .args(["-extract", "ProgramArguments.0", "raw", "-o", "-"])
        .arg(entry)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let executable = String::from_utf8(output.stdout).ok()?;
    let executable = executable.trim_end_matches(['\r', '\n']);
    (!executable.is_empty()).then(|| PathBuf::from(executable))
}

#[cfg(target_os = "macos")]
fn sync_macos_autostart_path(
    app: &AppHandle,
    home_directory: &Path,
    migration: &MacosBundleMigration,
) -> Result<(), String> {
    if !is_beta_release() {
        return Ok(());
    }

    let launch_agents = home_directory.join("Library/LaunchAgents");
    let stable_entry = launch_agents.join("Broadcast.plist");
    let migrate_stable_entry = macos_launch_agent_executable(&stable_entry).as_deref()
        == Some(migration.source_executable.as_path());

    if !migration.beta_autostart_was_enabled && !migrate_stable_entry {
        return Ok(());
    }
    app.autolaunch()
        .enable()
        .map_err(|error| format!("无法更新 Beta 开机自启动路径：{error}"))?;
    if !migrate_stable_entry {
        return Ok(());
    }
    match fs::remove_file(&stable_entry) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "无法移除已迁移的开机自启动项 {}：{error}",
            stable_entry.display()
        )),
    }
}

#[cfg(target_os = "macos")]
fn install_broadcast_symlink(app: &AppHandle) -> Result<(PathBuf, PathBuf), String> {
    let wrapper_path = app
        .path()
        .resource_dir()
        .map_err(|error| format!("无法定位当前 App 资源目录：{error}"))?
        .join("bin/broadcast");
    let wrapper_path = fs::canonicalize(&wrapper_path).map_err(|error| {
        format!(
            "无法定位当前 App 内置命令行工具 {}：{error}",
            wrapper_path.display()
        )
    })?;
    if !wrapper_path.is_file() {
        return Err(format!(
            "当前 App 内置命令行工具不是文件：{}",
            wrapper_path.display()
        ));
    }

    let system_path = PathBuf::from("/opt/homebrew/bin/broadcast");
    let temporary_path = system_path.with_file_name(format!(
        ".broadcast-broadcast-{}.tmp",
        Uuid::new_v4()
    ));
    std::os::unix::fs::symlink(&wrapper_path, &temporary_path).map_err(|error| {
        format!(
            "无法在 {} 创建符号链接：{error}",
            system_path.parent().unwrap().display()
        )
    })?;
    if let Err(error) = fs::rename(&temporary_path, &system_path) {
        let _ = fs::remove_file(&temporary_path);
        return Err(format!(
            "无法替换系统命令 {}：{error}",
            system_path.display()
        ));
    }

    let installed_target = fs::read_link(&system_path)
        .map_err(|error| format!("无法验证系统命令 {}：{error}", system_path.display()))?;
    if installed_target != wrapper_path {
        return Err(format!(
            "系统命令未指向当前 App：{} -> {}",
            system_path.display(),
            installed_target.display()
        ));
    }

    Ok((system_path, wrapper_path))
}

#[cfg(target_os = "macos")]
fn find_codex_app(home_directory: &Path) -> Option<PathBuf> {
    [
        PathBuf::from("/Applications/ChatGPT.app"),
        home_directory.join("Applications/ChatGPT.app"),
        PathBuf::from("/Applications/Codex.app"),
        home_directory.join("Applications/Codex.app"),
    ]
    .into_iter()
    .find(|candidate| candidate.is_dir())
}

#[cfg(target_os = "macos")]
fn ordinary_codex_process(app_path: &Path) -> Result<Option<u32>, String> {
    let app_name = app_path
        .file_stem()
        .ok_or_else(|| "无法识别 Codex App 名称".to_string())?;
    let executable = app_path.join("Contents/MacOS").join(app_name);
    let output = StdCommand::new("/bin/ps")
        .args(["-ww", "-axo", "pid=,command="])
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err("无法检查正在运行的 Codex".to_string());
    }

    let executable = executable.to_string_lossy();
    let mut ordinary_pid = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim_start();
        let Some(separator) = line.find(char::is_whitespace) else {
            continue;
        };
        let command = line[separator..].trim_start();
        if command != executable && !command.starts_with(&format!("{executable} ")) {
            continue;
        }
        if command.contains(" --remote-debugging-port=") {
            return Ok(None);
        }
        ordinary_pid = line[..separator].parse().ok();
    }
    Ok(ordinary_pid)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn process_is_running(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(target_os = "macos")]
fn quit_codex_normally(pid: u32) -> Result<(), String> {
    let application =
        NSRunningApplication::runningApplicationWithProcessIdentifier(pid as libc::pid_t)
            .ok_or_else(|| "无法找到正在运行的 Codex".to_string())?;
    if !application.terminate() {
        return Err("Codex 没有接受退出请求".to_string());
    }
    let deadline = Instant::now() + LAUNCHER_STOP_TIMEOUT;
    while process_is_running(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
    if process_is_running(pid) {
        return Err("Codex 尚未退出，任务面板没有启动".to_string());
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn find_codex_app(_home_directory: &Path) -> Option<PathBuf> {
    let output = StdCommand::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "(Get-AppxPackage -Name OpenAI.Codex).InstallLocation",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let install_location = String::from_utf8_lossy(&output.stdout);
    let candidate = PathBuf::from(install_location.trim())
        .join("app")
        .join("ChatGPT.exe");
    candidate.is_file().then_some(candidate)
}

#[cfg(target_os = "windows")]
fn ordinary_codex_process(app_path: &Path, codex_profile: &Path) -> Result<Option<u32>, String> {
    let output = StdCommand::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$ErrorActionPreference = 'Stop'; $app = $env:BROADCAST_CODEX_APP_PATH; $profile = $env:BROADCAST_CODEX_PROFILE; $name = [IO.Path]::GetFileName($app); $all = @(Get-CimInstance Win32_Process -Filter \"Name = '$name'\" | Where-Object { $_.ExecutablePath -eq $app }); $pids = @{}; foreach ($item in $all) { $pids[[uint32]$item.ProcessId] = $true }; $process = $all | Where-Object { $command = [string]$_.CommandLine; $isRoot = -not $pids.ContainsKey([uint32]$_.ParentProcessId); $usesManagedProfile = $command.IndexOf('--user-data-dir', [StringComparison]::OrdinalIgnoreCase) -ge 0 -and $command.IndexOf($profile, [StringComparison]::OrdinalIgnoreCase) -ge 0; $usesPrivateCdp = $command.IndexOf('--remote-debugging-pipe', [StringComparison]::OrdinalIgnoreCase) -ge 0 -or $command.IndexOf('--remote-debugging-port', [StringComparison]::OrdinalIgnoreCase) -ge 0; $isManaged = $usesManagedProfile -and $usesPrivateCdp; $isRoot -and -not $isManaged } | Select-Object -First 1; if ($null -ne $process) { [Console]::Out.Write($process.ProcessId) }",
        ])
        .env("BROADCAST_CODEX_APP_PATH", app_path)
        .env("BROADCAST_CODEX_PROFILE", codex_profile)
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err("无法检查正在运行的 Codex".to_string());
    }
    let pid = String::from_utf8_lossy(&output.stdout);
    let pid = pid.trim();
    if pid.is_empty() {
        return Ok(None);
    }
    pid.parse()
        .map(Some)
        .map_err(|_| "无法检查正在运行的 Codex".to_string())
}

#[cfg(target_os = "windows")]
fn quit_codex_normally(pid: u32) -> Result<(), String> {
    let process = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            pid,
        )
    }
    .map_err(|error| error.to_string())?;
    let mut creation_time = FILETIME::default();
    let mut exit_time = FILETIME::default();
    let mut kernel_time = FILETIME::default();
    let mut user_time = FILETIME::default();
    if unsafe {
        GetProcessTimes(
            process,
            &mut creation_time,
            &mut exit_time,
            &mut kernel_time,
            &mut user_time,
        )
    }
    .is_err()
    {
        let _ = unsafe { CloseHandle(process) };
        return Err("无法检查正在运行的 Codex".to_string());
    }

    let mut session = 0;
    let mut session_key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
    let started = unsafe { RmStartSession(&mut session, None, PWSTR(session_key.as_mut_ptr())) };
    if started != ERROR_SUCCESS {
        let _ = unsafe { CloseHandle(process) };
        return Err("无法请求 Codex 退出".to_string());
    }
    let application = RM_UNIQUE_PROCESS {
        dwProcessId: pid,
        ProcessStartTime: creation_time,
    };
    let registered = unsafe { RmRegisterResources(session, None, Some(&[application]), None) };
    let shutdown = if registered == ERROR_SUCCESS {
        unsafe { RmShutdown(session, 0, None) }
    } else {
        registered
    };
    let _ = unsafe { RmEndSession(session) };
    if shutdown != ERROR_SUCCESS {
        let _ = unsafe { CloseHandle(process) };
        return Err("Codex 没有接受退出请求".to_string());
    }

    let exited = unsafe {
        WaitForSingleObject(
            process,
            LAUNCHER_STOP_TIMEOUT.as_millis().try_into().unwrap(),
        )
    } == WAIT_OBJECT_0;
    let _ = unsafe { CloseHandle(process) };
    if exited {
        Ok(())
    } else {
        Err("Codex 尚未退出，任务面板没有启动".to_string())
    }
}

#[cfg(target_os = "linux")]
fn find_codex_app(_home_directory: &Path) -> Option<PathBuf> {
    let candidate = PathBuf::from("/usr/lib/chatgpt/ChatGPT");
    candidate.is_file().then_some(candidate)
}

#[cfg(target_os = "linux")]
fn ordinary_codex_process(app_path: &Path, codex_profile: &Path) -> Result<Option<u32>, String> {
    let output = StdCommand::new("/bin/ps")
        .args(["-ww", "-axo", "pid=,ppid=,command="])
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err("无法检查正在运行的 Codex".to_string());
    }

    let executable = app_path.to_string_lossy();
    let mut processes = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim_start();
        let Some(pid_separator) = line.find(char::is_whitespace) else {
            continue;
        };
        let Some(pid) = line[..pid_separator].parse::<u32>().ok() else {
            continue;
        };
        let parent_and_command = line[pid_separator..].trim_start();
        let Some(parent_separator) = parent_and_command.find(char::is_whitespace) else {
            continue;
        };
        let Some(parent_pid) = parent_and_command[..parent_separator].parse::<u32>().ok() else {
            continue;
        };
        let command = parent_and_command[parent_separator..].trim_start();
        if command != executable && !command.starts_with(&format!("{executable} ")) {
            continue;
        }
        processes.push((pid, parent_pid, command.to_string()));
    }

    let managed_profile = format!("--user-data-dir={}", codex_profile.display());
    Ok(processes
        .iter()
        .find(|(pid, parent_pid, command)| {
            !processes
                .iter()
                .any(|(candidate_pid, _, _)| candidate_pid == parent_pid && candidate_pid != pid)
                && !(command.contains(" --remote-debugging-pipe")
                    && command.contains(&format!(" {managed_profile}")))
        })
        .map(|(pid, _, _)| *pid))
}

#[cfg(target_os = "linux")]
fn quit_codex_normally(pid: u32) -> Result<(), String> {
    if unsafe { libc::kill(pid as i32, libc::SIGTERM) } != 0 {
        return Err("Codex 没有接受退出请求".to_string());
    }
    let deadline = Instant::now() + LAUNCHER_STOP_TIMEOUT;
    while process_is_running(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
    if process_is_running(pid) {
        return Err("Codex 尚未退出，任务面板没有启动".to_string());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn missing_codex_app_message() -> String {
    "未找到官方 ChatGPT.app 或 Codex.app。请先安装到 Applications 文件夹。".to_string()
}

#[cfg(target_os = "windows")]
fn missing_codex_app_message() -> String {
    "未找到官方 Codex App。请先从 Microsoft Store 安装。".to_string()
}

#[cfg(target_os = "linux")]
fn missing_codex_app_message() -> String {
    "未找到官方 ChatGPT App。请先安装 Ubuntu x64 .deb。".to_string()
}

fn stop_service_locked(app: &AppHandle, state: &Arc<LauncherState>) {
    state.generation.fetch_add(1, Ordering::SeqCst);
    state.intentional_stop.store(true, Ordering::SeqCst);
    let service = state.service.lock().unwrap().take();
    if let Some(service) = service {
        append_log(state, "Stopping Broadcast service");
        service.launcher.stop();
        if service.stopped.recv_timeout(LAUNCHER_STOP_TIMEOUT).is_err() {
            append_log(state, "Broadcast service did not stop in time");
        }
    }
    update_snapshot(app, state, |snapshot| {
        snapshot.phase = "stopped".into();
        snapshot.message = "任务面板已停止。".into();
    });
}

fn stop_service(app: &AppHandle, state: &Arc<LauncherState>) {
    let _lifecycle = state.lifecycle.lock().unwrap();
    stop_service_locked(app, state);
}

/// The server or the launcher of `generation` ended on its own: restart both, as the
/// supervisor of the old Node child did.
fn service_exited(app: AppHandle, state: Arc<LauncherState>, generation: u64) {
    let recovery_token = generation + 1;
    {
        let mut service = state.service.lock().unwrap();
        if service.as_ref().map(|service| service.generation) != Some(generation)
            || state
                .generation
                .compare_exchange(generation, recovery_token, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return;
        }
        *service = None;
    }
    update_snapshot(&app, &state, |snapshot| {
        if state.generation.load(Ordering::SeqCst) == recovery_token {
            snapshot.phase = "error".into();
            snapshot.message = "任务面板进程已退出，正在恢复…".into();
        }
    });
    thread::sleep(Duration::from_secs(2));
    let (recovery_result, recovery_generation) = {
        let _lifecycle = state.lifecycle.lock().unwrap();
        if state.generation.load(Ordering::SeqCst) != recovery_token
            || state.intentional_stop.load(Ordering::SeqCst)
            || state.update_in_progress.load(Ordering::SeqCst)
        {
            return;
        }
        let result = start_launcher_locked(&app, &state);
        (result, state.generation.load(Ordering::SeqCst))
    };
    if let Err(error) = recovery_result {
        append_log(&state, &format!("Launcher recovery failed: {error}"));
        let stopped = state.service.lock().unwrap().is_none();
        update_snapshot(&app, &state, |snapshot| {
            if state.generation.load(Ordering::SeqCst) == recovery_generation && stopped {
                snapshot.phase = "error".into();
                snapshot.message = error.clone();
            }
        });
        show_error_dialog(
            &app,
            "Broadcast 恢复失败",
            &format!("任务面板进程无法恢复：{error}\n\n请重新打开 App。"),
        );
    }
}

fn start_launcher_locked(
    app: &AppHandle,
    state: &Arc<LauncherState>,
) -> Result<LauncherSnapshot, String> {
    if state.service.lock().unwrap().is_some() {
        return Ok(state.snapshot.lock().unwrap().clone());
    }

    let home_directory = app.path().home_dir().map_err(|error| error.to_string())?;
    // Development builds can point at a stand-in app instead of the installed Codex.
    #[cfg(debug_assertions)]
    let codex_app = std::env::var_os("BROADCAST_CODEX_APP")
        .map(PathBuf::from)
        .or_else(|| find_codex_app(&home_directory));
    #[cfg(not(debug_assertions))]
    let codex_app = find_codex_app(&home_directory);
    let codex_app = codex_app.ok_or_else(missing_codex_app_message)?;
    let resource_directory = app
        .path()
        .resource_dir()
        .map_err(|error| error.to_string())?;
    let cli_path = std::env::current_exe()
        .map_err(|error| error.to_string())?
        .parent()
        .ok_or_else(|| "无法定位 App 可执行文件目录".to_string())?
        .join(format!("broadcast-cli{}", std::env::consts::EXE_SUFFIX));
    let codex_profile = state.data_directory.join("codex-profile");
    #[cfg(target_os = "macos")]
    let ordinary_codex_pid = ordinary_codex_process(&codex_app)?;
    #[cfg(target_os = "windows")]
    let ordinary_codex_pid = ordinary_codex_process(&codex_app, &codex_profile)?;
    #[cfg(target_os = "linux")]
    let ordinary_codex_pid = ordinary_codex_process(&codex_app, &codex_profile)?;
    if let Some(codex_pid) = ordinary_codex_pid {
        let restart = app
            .dialog()
            .message("需要重新启动 Codex 才能显示任务面板")
            .title("Broadcast")
            .kind(MessageDialogKind::Info)
            .buttons(MessageDialogButtons::OkCancelCustom(
                "重新启动 Codex".into(),
                "取消".into(),
            ))
            .blocking_show();
        if !restart {
            append_log(state, "Codex restart canceled by user");
            return Ok(update_snapshot(app, state, |snapshot| {
                snapshot.phase = "stopped".into();
                snapshot.message = "已取消重新启动 Codex，任务面板未注入。".into();
                snapshot.app_path = Some(codex_app.display().to_string());
            }));
        }
        append_log(
            state,
            &format!("Requesting normal Codex exit for PID {codex_pid}"),
        );
        quit_codex_normally(codex_pid)?;
    }
    let generation = state.generation.fetch_add(1, Ordering::SeqCst) + 1;
    state.intentional_stop.store(false, Ordering::SeqCst);
    update_snapshot(app, state, |snapshot| {
        snapshot.phase = "starting".into();
        snapshot.message = "正在启动任务面板服务…".into();
        snapshot.app_path = Some(codex_app.display().to_string());
    });

    let listener = broadcast_listener(state)?;
    let broadcast_port = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let codex_port = codex_port(state)?;
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let cdp = broadcast_codex::Cdp::Port(codex_port);
    #[cfg(target_os = "linux")]
    let cdp = broadcast_codex::Cdp::Pipe;
    let instance_token = Uuid::new_v4().to_string();
    let instance_secret = Uuid::new_v4().to_string();
    let version = state.snapshot.lock().unwrap().version.clone();
    #[cfg(target_os = "macos")]
    let codex_source_profile = home_directory.join("Library/Application Support/Codex");
    #[cfg(target_os = "windows")]
    let codex_source_profile = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| "APPDATA is unavailable".to_string())?
        .join("Codex/web/Codex");
    #[cfg(target_os = "linux")]
    let codex_source_profile = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_directory.join(".config"))
        .join("Codex");
    let codex_executable = broadcast_codex::runnable_executable(
        broadcast_codex::executable(
            std::env::var("CODEX_EXECUTABLE").ok().as_deref(),
            Some(&codex_app),
        ),
        &state.data_directory,
    )
    .map_err(|error| format!("无法准备 Codex CLI：{error}"))?;
    let config = broadcast_server::Config {
        data_dir: state.data_directory.clone(),
        static_dir: resource_directory.join("web"),
        skill_path: home_directory.join(".agents/skills/manage-broadcast/SKILL.md"),
        codex_home: std::env::var_os("CODEX_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home_directory.join(".codex")),
        codex_executable: codex_executable.clone(),
        host: Ipv4Addr::LOCALHOST.into(),
        port: broadcast_port,
        trusted_origins: HashSet::new(),
        launcher: Some(broadcast_server::Launcher {
            token: instance_token.clone(),
            secret: instance_secret.clone(),
        }),
        version,
    };
    let options = broadcast_codex::Options {
        app_path: codex_app,
        cdp,
        profile: codex_profile,
        source_profile: Some(codex_source_profile),
        data_dir: state.data_directory.clone(),
        port: broadcast_port,
        token: instance_token,
        secret: instance_secret,
        open: true,
        cli: cli_path,
        runtime_file: Some(state.data_directory.join("launcher-runtime.json")),
        codex_executable,
    };

    let runtime = tauri::async_runtime::handle();
    let _runtime = runtime.inner().enter();
    let mut server =
        broadcast_server::Server::open(config).map_err(|error| format!("{error:#}"))?;
    let bridge = server
        .host_bridge()
        .expect("a new server still has its host bridge");
    listener
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;
    let listener =
        tokio::net::TcpListener::from_std(listener).map_err(|error| error.to_string())?;
    let (shutdown, shutdown_requested) = tokio::sync::oneshot::channel::<()>();
    let mut serving = tokio::spawn(server.serve(listener, async move {
        let _ = shutdown_requested.await;
    }));
    update_snapshot(app, state, |snapshot| {
        snapshot.message = "任务面板服务已启动，正在注入 Codex…".into();
    });
    let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
    let (launcher, mut launching) = broadcast_codex::Launcher::start(
        options,
        bridge.requests,
        bridge.notifications,
        events,
    );
    let (stopped_sender, stopped) = std::sync::mpsc::channel();
    *state.service.lock().unwrap() = Some(Service {
        generation,
        launcher: launcher.clone(),
        stopped,
    });

    let service_app = app.clone();
    let service_state = Arc::clone(state);
    tokio::spawn(async move {
        let (launched, served) = tokio::select! {
            launched = &mut launching => {
                let _ = shutdown.send(());
                (launched, serving.await)
            }
            served = &mut serving => {
                launcher.stop();
                (launching.await, served)
            }
        };
        append_log(
            &service_state,
            &format!("Broadcast service exited: launcher {launched:?}, server {served:?}"),
        );
        let _ = stopped_sender.send(());
        thread::spawn(move || service_exited(service_app, service_state, generation));
    });

    let event_app = app.clone();
    let event_state = Arc::clone(state);
    tokio::spawn(async move {
        while let Some(event) = received.recv().await {
            let (phase, message) = match event {
                broadcast_codex::Event::WaitingForCodex => ("starting", "正在等待 Codex 窗口…"),
                broadcast_codex::Event::Injected => ("running", "任务面板已在 Codex 客户端中打开。"),
                broadcast_codex::Event::OpenedInExistingCodex => {
                    ("running", "任务面板已在现有 Codex 的浏览面板中打开。")
                }
                broadcast_codex::Event::OpenSignalQueued => continue,
            };
            update_snapshot(&event_app, &event_state, |snapshot| {
                if event_state.generation.load(Ordering::SeqCst) == generation {
                    snapshot.phase = phase.into();
                    snapshot.message = message.into();
                }
            });
        }
    });

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    append_log(
        state,
        &format!("Started Broadcast on {broadcast_port} with Codex CDP {codex_port}"),
    );
    #[cfg(target_os = "linux")]
    append_log(
        state,
        &format!("Started Broadcast on {broadcast_port} with a private Codex CDP pipe"),
    );
    Ok(state.snapshot.lock().unwrap().clone())
}

fn start_launcher(app: &AppHandle, state: &Arc<LauncherState>) -> Result<LauncherSnapshot, String> {
    let _lifecycle = state.lifecycle.lock().unwrap();
    if state.intentional_stop.load(Ordering::SeqCst)
        || state.update_in_progress.load(Ordering::SeqCst)
    {
        return Ok(state.snapshot.lock().unwrap().clone());
    }
    start_launcher_locked(app, state)
}

fn restart_launcher(
    app: &AppHandle,
    state: &Arc<LauncherState>,
) -> Result<LauncherSnapshot, String> {
    let (result, result_generation) = {
        let _lifecycle = state.lifecycle.lock().unwrap();
        if state.intentional_stop.load(Ordering::SeqCst) {
            return Ok(state.snapshot.lock().unwrap().clone());
        }
        if state.update_in_progress.load(Ordering::SeqCst) {
            append_log(state, "Launcher reopen ignored during update installation");
            return Ok(state.snapshot.lock().unwrap().clone());
        }
        stop_service_locked(app, state);
        let result = start_launcher_locked(app, state);
        state.intentional_stop.store(false, Ordering::SeqCst);
        let generation = state.generation.load(Ordering::SeqCst);
        (result, generation)
    };
    if let Err(error) = &result {
        let error = error.clone();
        let stopped = state.service.lock().unwrap().is_none();
        update_snapshot(app, state, |snapshot| {
            if state.generation.load(Ordering::SeqCst) == result_generation && stopped {
                snapshot.phase = "error".into();
                snapshot.message = format!("任务面板启动失败：{error}");
            }
        });
    }
    result
}

fn open_broadcast(state: &LauncherState) {
    if let Some(service) = state.service.lock().unwrap().as_ref() {
        service.launcher.open();
    }
}

fn open_broadcast_in_browser(state: &LauncherState) -> Result<(), String> {
    let descriptor = fs::read_to_string(state.data_directory.join("launcher-runtime.json"))
        .map_err(|error| error.to_string())?;
    let descriptor: LauncherRuntimeDescriptor =
        serde_json::from_str(&descriptor).map_err(|error| error.to_string())?;
    let url = format!("{}/", descriptor.url.trim_end_matches('/'));
    #[cfg(target_os = "macos")]
    let status = StdCommand::new("/usr/bin/open")
        .arg(&url)
        .status()
        .map_err(|error| error.to_string())?;
    #[cfg(target_os = "windows")]
    let status = StdCommand::new("rundll32.exe")
        .args(["url.dll,FileProtocolHandler", &url])
        .status()
        .map_err(|error| error.to_string())?;
    #[cfg(target_os = "linux")]
    let status = StdCommand::new("xdg-open")
        .arg(&url)
        .status()
        .map_err(|error| error.to_string())?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| "系统默认浏览器没有打开任务面板".to_string())
}

async fn check_for_startup_update(
    app: &AppHandle,
    state: &Arc<LauncherState>,
) -> Result<Option<Update>, String> {
    update_snapshot(app, state, |snapshot| {
        snapshot.update_message = "正在检查更新…".into();
        snapshot.update_available = false;
    });
    let beta_release = is_beta_release();
    let current_release_version = if beta_release {
        release_version()
            .parse()
            .map_err(|error| format!("Invalid release version {}: {error}", release_version()))?
    } else {
        app.package_info().version.clone()
    };
    let mut updater_builder = app.updater_builder();
    if beta_release {
        let beta_endpoint = BETA_UPDATER_ENDPOINT
            .parse()
            .map_err(|error| format!("Invalid Beta updater endpoint: {error}"))?;
        updater_builder = updater_builder
            .endpoints(vec![beta_endpoint])
            .map_err(|error| error.to_string())?;
    }
    let update = updater_builder
        .version_comparator(move |current, release| {
            if beta_release {
                release.version > current_release_version
            } else {
                release.version > current
            }
        })
        .build()
        .map_err(|error| error.to_string())?
        .check()
        .await
        .map_err(|error| error.to_string())?;
    match &update {
        Some(update) => {
            append_log(state, &format!("Update {} is available", update.version));
            update_snapshot(app, state, |snapshot| {
                snapshot.update_message =
                    format!("发现新版本 {}，可以下载并安装。", update.version);
                snapshot.update_available = true;
            });
        }
        None => {
            append_log(state, "No update is available");
            update_snapshot(app, state, |snapshot| {
                snapshot.update_message =
                    format!("当前版本 {} 已是最新版本。", snapshot.version.as_str());
                snapshot.update_available = false;
            });
        }
    }
    Ok(update)
}

async fn prepare_update(
    app: &AppHandle,
    state: &Arc<LauncherState>,
    update: &Update,
    dialog: &Arc<Mutex<Option<UpdateDialog>>>,
) -> Result<Vec<u8>, String> {
    let update_version = update.version.clone();
    append_log(
        state,
        &format!("Downloading update {update_version} before confirmation"),
    );
    update_snapshot(app, state, |snapshot| {
        snapshot.update_message = format!("正在下载 {update_version}…");
        snapshot.update_available = true;
    });
    let progress_app = app.clone();
    let progress_state = Arc::clone(state);
    let progress_version = update_version.clone();
    let finish_app = app.clone();
    let finish_state = Arc::clone(state);
    let progress_dialog = Arc::clone(dialog);
    let finish_dialog = Arc::clone(dialog);
    let mut downloaded = 0_u64;
    let mut displayed_progress = None;
    let bytes = update.download(
        move |chunk_length, content_length| {
            downloaded = downloaded.saturating_add(chunk_length as u64);
            let progress = content_length.filter(|total| *total > 0).map(|total| {
                downloaded
                    .saturating_mul(100)
                    .saturating_div(total)
                    .min(100)
            });
            if progress == displayed_progress {
                return;
            }
            displayed_progress = progress;
            let snapshot = update_snapshot(&progress_app, &progress_state, |snapshot| {
                snapshot.update_message = match progress {
                    Some(progress) => format!("正在下载 {progress_version} · {progress}%"),
                    None => format!("正在下载 {progress_version}…"),
                };
            });
            let dialog = progress_dialog.lock().unwrap().clone();
            if let Some(dialog) = dialog {
                dialog.set_progress(&snapshot.update_message, progress, false);
            }
        },
        move || {
            let snapshot = update_snapshot(&finish_app, &finish_state, |snapshot| {
                snapshot.update_message = "正在验证更新…".into();
            });
            let dialog = finish_dialog.lock().unwrap().clone();
            if let Some(dialog) = dialog {
                dialog.set_progress(&snapshot.update_message, None, false);
            }
        },
    )
    .await
    .map_err(|error| error.to_string())?;

    append_log(
        state,
        &format!("Downloaded and verified update {update_version}"),
    );
    update_snapshot(app, state, |snapshot| {
        snapshot.update_message = format!("{update_version} 已下载并通过签名验证，等待安装。");
        snapshot.update_available = true;
    });
    Ok(bytes)
}

async fn prepare_available_update(
    app: &AppHandle,
    state: &Arc<LauncherState>,
    refresh: bool,
) -> Result<Option<PendingUpdate>, String> {
    let mut pending = state.pending_update.lock().await;
    if let Some(current) = pending.as_ref() {
        if matches!(current.download.peek(), Some(Err(_))) {
            *pending = None;
        } else if !refresh || current.download.peek().is_none() {
            return Ok(Some(current.clone()));
        }
    }
    let Some(update) = check_for_startup_update(app, state).await? else {
        *pending = None;
        return Ok(None);
    };
    if let Some(current) = pending.as_ref()
        && current.update.version == update.version {
            return Ok(Some(current.clone()));
        }

    let dialog = Arc::new(Mutex::new(None));
    let download = {
        let app = app.clone();
        let state = Arc::clone(state);
        let update = update.clone();
        let dialog = Arc::clone(&dialog);
        async move {
            let result = prepare_update(&app, &state, &update, &dialog).await;
            if let Err(error) = &result {
                append_log(&state, &format!("Update {} preparation failed: {error}", update.version));
                update_snapshot(&app, &state, |snapshot| {
                    snapshot.update_message = format!("更新下载或签名验证失败：{error}");
                    snapshot.update_available = true;
                });
            }
            result.map(Arc::new)
        }.boxed().shared()
    };
    tauri::async_runtime::spawn(download.clone());
    let prepared = PendingUpdate { update, download, dialog };
    *pending = Some(prepared.clone());
    Ok(Some(prepared))
}

fn install_update(
    app: &AppHandle,
    state: &Arc<LauncherState>,
    update: Update,
    bytes: &[u8],
    update_dialog: &UpdateDialog,
) -> Result<(), String> {
    let update_version = update.version.clone();
    state.update_in_progress.store(true, Ordering::SeqCst);

    let snapshot = update_snapshot(app, state, |snapshot| {
        snapshot.update_message = "正在安装更新…".into();
        snapshot.update_available = false;
    });
    update_dialog.show_installing(&snapshot.update_message);
    {
        let _lifecycle = state.lifecycle.lock().unwrap();
        if state.intentional_stop.load(Ordering::SeqCst) {
            return Err("App exit is in progress".into());
        }
        stop_service_locked(app, state);
    }
    if let Err(error) = update.install(bytes) {
        append_log(state, &format!("Update installation failed: {error}"));
        let restart_error = {
            let _lifecycle = state.lifecycle.lock().unwrap();
            let restart_error = start_launcher_locked(app, state).err();
            state.intentional_stop.store(false, Ordering::SeqCst);
            state.update_in_progress.store(false, Ordering::SeqCst);
            restart_error
        };
        if let Some(restart_error) = &restart_error {
            append_log(
                state,
                &format!("Broadcast restart after update failure failed: {restart_error}"),
            );
        } else {
            append_log(
                state,
                "Broadcast restarted after update installation failure",
            );
        }
        update_snapshot(app, state, |snapshot| {
            snapshot.update_message = format!("更新安装失败：{error}");
            snapshot.update_available = true;
            if let Some(restart_error) = &restart_error {
                snapshot.phase = "error".into();
                snapshot.message = format!("任务面板恢复失败：{restart_error}");
            }
        });
        return Err(error.to_string());
    }

    append_log(
        state,
        &format!("Installed update {update_version}; restarting"),
    );
    let snapshot = update_snapshot(app, state, |snapshot| {
        snapshot.update_message = "正在重启…".into();
    });
    update_dialog.set_progress(&snapshot.update_message, None, false);
    app.restart()
}

fn finish_update_flow(
    state: &LauncherState,
    check_update: &MenuItem<tauri::Wry>,
    quit: &MenuItem<tauri::Wry>,
) {
    state.update_in_progress.store(false, Ordering::SeqCst);
    check_update.set_text("检查更新").unwrap();
    check_update.set_enabled(true).unwrap();
    quit.set_enabled(true).unwrap();
    state.update_flow_in_progress.store(false, Ordering::SeqCst);
}

async fn offer_update(
    app: &AppHandle,
    state: &Arc<LauncherState>,
    check_update: &MenuItem<tauri::Wry>,
    quit: &MenuItem<tauri::Wry>,
    show_current_version: bool,
) {
    if cfg!(target_os = "windows") {
        update_snapshot(app, state, |snapshot| {
            snapshot.update_message = "Windows 版本暂不支持自动更新。".into();
            snapshot.update_available = false;
        });
        check_update
            .set_text("检查更新（Windows 暂不支持）")
            .unwrap();
        check_update.set_enabled(false).unwrap();
        return;
    }
    if show_current_version {
        if state
            .update_flow_in_progress
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        check_update.set_text("正在检查更新…").unwrap();
        check_update.set_enabled(false).unwrap();
    } else if state.update_flow_in_progress.load(Ordering::SeqCst) {
        return;
    }
    let update = match prepare_available_update(app, state, !show_current_version).await {
        Ok(update) => update,
        Err(error) => {
            append_log(state, &format!("Update check failed: {error}"));
            update_snapshot(app, state, |snapshot| {
                snapshot.update_message = format!("更新检查失败：{error}");
                snapshot.update_available = false;
            });
            if show_current_version {
                show_error_dialog(
                    app,
                    "Broadcast 更新检查失败",
                    &format!("无法检查更新。请稍后重试。\n\n{error}"),
                );
                finish_update_flow(state, check_update, quit);
            }
            return;
        }
    };
    if !show_current_version {
        return;
    }
    let Some(update) = update else {
        let current_version = state.snapshot.lock().unwrap().version.clone();
        app.dialog()
            .message(format!("当前版本 {current_version} 已是最新版本。"))
            .title("Broadcast 更新")
            .buttons(MessageDialogButtons::Ok)
            .blocking_show();
        finish_update_flow(state, check_update, quit);
        return;
    };

    let version = update.update.version.clone();
    append_log(state, &format!("Showing update prompt for {version}"));
    let Some(update_dialog) = UpdateDialog::prompt(app, &version) else {
        append_log(state, &format!("Update {version} deferred by user"));
        update_snapshot(app, state, |snapshot| {
            snapshot.update_message =
                format!("已暂缓安装 {version}，可稍后从检查更新继续。");
            snapshot.update_available = true;
        });
        finish_update_flow(state, check_update, quit);
        return;
    };
    append_log(state, &format!("Update {version} accepted by user"));
    if update.download.peek().is_none() {
        update_dialog.set_progress("正在下载或验证更新…", None, false);
    }
    *update.dialog.lock().unwrap() = Some(update_dialog.clone());
    let result = update.download.clone().await;
    *update.dialog.lock().unwrap() = None;
    let bytes = match result {
        Ok(bytes) => bytes,
        Err(error) => {
            update_dialog.close();
            show_error_dialog(
                app,
                "Broadcast 更新准备失败",
                &format!("无法下载或验证更新。请稍后重试。\n\n{error}"),
            );
            finish_update_flow(state, check_update, quit);
            return;
        }
    };
    quit.set_enabled(false).unwrap();
    match install_update(app, state, update.update, &bytes, &update_dialog) {
        Ok(()) => {
            update_dialog.close();
            finish_update_flow(state, check_update, quit);
        }
        Err(error) => {
            append_log(state, &format!("Update installation failed: {error}"));
            let service_recovered = state.service.lock().unwrap().is_some();
            let service_message = if service_recovered {
                "任务面板服务已恢复。"
            } else {
                "任务面板服务未能恢复，请重新打开 App。"
            };
            update_dialog.close();
            show_error_dialog(
                app,
                "Broadcast 更新失败",
                &format!(
                    "更新未完成。{service_message}\n\n请稍后重试。详情见启动日志。\n\n{error}"
                ),
            );
            finish_update_flow(state, check_update, quit);
        }
    }
}

/// The embedded server and launcher inherit the environment the Node child was given: the
/// app's `bin` first on `PATH`, no `CODEX_API_KEY`, and the resources as working directory.
fn configure_process(resource_directory: &Path) {
    #[cfg(target_os = "macos")]
    let path = std::env::join_paths(
        std::iter::once(resource_directory.join("bin")).chain(
            ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"]
                .map(PathBuf::from),
        ),
    );
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    let path = std::env::join_paths(
        std::iter::once(resource_directory.join("bin"))
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())),
    );
    // SAFETY: `main` calls this before Tauri or tokio start any thread.
    unsafe {
        if let Ok(path) = path {
            std::env::set_var("PATH", path);
        }
        std::env::remove_var("CODEX_API_KEY");
    }
    let _ = std::env::set_current_dir(resource_directory);
}

/// The embedded server and launcher print their diagnostics; the launcher log keeps them, as
/// it kept the Node child's output.
fn redirect_output(log_path: &Path) -> std::io::Result<()> {
    let log = OpenOptions::new().create(true).append(true).open(log_path)?;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    unsafe {
        if libc::dup2(log.as_raw_fd(), libc::STDOUT_FILENO) < 0
            || libc::dup2(log.as_raw_fd(), libc::STDERR_FILENO) < 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::io::IntoRawHandle;
        use windows::Win32::{
            Foundation::HANDLE,
            System::Console::{SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE},
        };
        let handle = HANDLE(log.into_raw_handle());
        unsafe {
            SetStdHandle(STD_OUTPUT_HANDLE, handle).map_err(std::io::Error::other)?;
            SetStdHandle(STD_ERROR_HANDLE, handle).map_err(std::io::Error::other)?;
        }
    }
    Ok(())
}

fn main() {
    let mut arguments = std::env::args_os().skip(1);
    if arguments
        .next()
        .is_some_and(|argument| argument == broadcast_server::TURN_OWNER_ARG)
    {
        broadcast_server::turn_owner(arguments);
    }
    let context = tauri::generate_context!();
    if let Ok(resource_directory) =
        tauri::utils::platform::resource_dir(context.package_info(), &tauri::Env::default())
    {
        configure_process(&resource_directory);
    }
    #[cfg(target_os = "macos")]
    let macos_bundle_migration = match migrate_macos_beta_app_bundle_name() {
        Ok(migration) => migration,
        Err(error) => {
            append_macos_startup_log(&format!("macOS App bundle migration failed: {error}"));
            None
        }
    };

    let app = tauri::Builder::default()
        .enable_macos_default_menu(false)
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(ActivationPolicy::Accessory);
            let home_directory = app.path().home_dir()?;
            let bundled_skill = app
                .path()
                .resource_dir()?
                .join("skills/manage-broadcast");
            let legacy_skill_conflict = reconcile_legacy_skill(&home_directory, &bundled_skill)?;
            let global_skill = home_directory.join(".agents/skills/manage-broadcast");
            if global_skill.exists() {
                fs::remove_dir_all(&global_skill)?;
            }
            copy_directory(&bundled_skill, &global_skill)?;
            #[cfg(target_os = "macos")]
            let data_directory = home_directory.join("Library/Application Support/Broadcast");
            #[cfg(target_os = "macos")]
            let log_directory = home_directory.join("Library/Logs/Broadcast");
            #[cfg(target_os = "windows")]
            let data_directory = std::env::var_os("APPDATA")
                .map(PathBuf::from)
                .ok_or_else(|| std::io::Error::other("APPDATA is unavailable"))?
                .join("Broadcast");
            #[cfg(target_os = "windows")]
            let log_directory = std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .ok_or_else(|| std::io::Error::other("LOCALAPPDATA is unavailable"))?
                .join("Broadcast/Logs");
            #[cfg(target_os = "linux")]
            let data_directory = std::env::var_os("XDG_DATA_HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home_directory.join(".local/share"))
                .join("Broadcast");
            #[cfg(target_os = "linux")]
            let log_directory = std::env::var_os("XDG_STATE_HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home_directory.join(".local/state"))
                .join("Broadcast");
            fs::create_dir_all(&data_directory)?;
            fs::create_dir_all(&log_directory)?;
            let Some(instance_lock) = acquire_instance_lock(&data_directory.join("launcher.lock"))?
            else {
                app.handle().exit(0);
                return Ok(());
            };
            let version = release_version().to_string();
            let state = Arc::new(LauncherState::new(
                data_directory,
                log_directory,
                version.clone(),
                instance_lock,
            ));
            redirect_output(&state.log_path)?;
            app.manage(state.clone());
            #[cfg(target_os = "macos")]
            if let Some(migration) = macos_bundle_migration.as_ref()
                && let Err(error) =
                    sync_macos_autostart_path(app.handle(), &home_directory, migration)
                {
                    append_log(&state, &format!("autostart path sync failed: {error}"));
                }
            // A development build must not repoint the installed app's command.
            #[cfg(target_os = "macos")]
            if !cfg!(debug_assertions)
                && let Err(error) = install_broadcast_symlink(app.handle()) {
                    append_log(&state, &format!("broadcast sync failed: {error}"));
                }

            let app_info = MenuItem::with_id(
                app,
                "app-info",
                format!("{} - {version}", app.package_info().name),
                false,
                None::<&str>,
            )?;
            let launcher_status = MenuItem::with_id(
                app,
                "launcher-status",
                "运行状态：启动中",
                false,
                None::<&str>,
            )?;
            *state.status_menu.lock().unwrap() = Some(launcher_status.clone());
            let open_broadcast_item =
                MenuItem::with_id(app, "open-broadcast", "打开任务面板", true, None::<&str>)?;
            let open_broadcast_web = MenuItem::with_id(
                app,
                "open-broadcast-web",
                "在网页打开任务面板",
                true,
                None::<&str>,
            )?;
            let check_update =
                MenuItem::with_id(app, "check-update", "检查更新", true, None::<&str>)?;
            let restart_codex =
                MenuItem::with_id(app, "restart-codex", "重新打开 Codex", true, None::<&str>)?;
            let autostart_enabled = app.autolaunch().is_enabled()?;
            let autostart = CheckMenuItem::with_id(
                app,
                "autostart",
                "开机自启动",
                true,
                autostart_enabled,
                None::<&str>,
            )?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            #[cfg(target_os = "macos")]
            let tray_menu = Menu::with_items(
                app,
                &[
                    &app_info,
                    &launcher_status,
                    &open_broadcast_item,
                    &open_broadcast_web,
                    &restart_codex,
                    &check_update,
                    &autostart,
                    &quit,
                ],
            )?;
            #[cfg(not(target_os = "macos"))]
            let tray_menu = Menu::with_items(
                app,
                &[
                    &app_info,
                    &launcher_status,
                    &open_broadcast_item,
                    &open_broadcast_web,
                    &restart_codex,
                    &check_update,
                    &autostart,
                    &quit,
                ],
            )?;
            let check_update_menu = check_update.clone();
            let quit_menu = quit.clone();
            let autostart_menu = autostart.clone();
            let autostart_confirmed = Arc::new(AtomicBool::new(autostart_enabled));
            TrayIconBuilder::new()
                .icon(tauri::include_image!("icons/tray-codex.png"))
                .icon_as_template(true)
                .tooltip("Broadcast")
                .menu(&tray_menu)
                .on_menu_event(move |app, event| match event.id().as_ref() {
                    "check-update" => {
                        let Some(state) = app.try_state::<Arc<LauncherState>>() else {
                            return;
                        };
                        let state = Arc::clone(state.inner());
                        let app = app.clone();
                        let check_update = check_update_menu.clone();
                        let quit = quit_menu.clone();
                        tauri::async_runtime::spawn(async move {
                            offer_update(&app, &state, &check_update, &quit, true).await;
                        });
                    }
                    "open-broadcast" => {
                        let Some(state) = app.try_state::<Arc<LauncherState>>() else {
                            return;
                        };
                        open_broadcast(&state);
                    }
                    "open-broadcast-web" => {
                        let Some(state) = app.try_state::<Arc<LauncherState>>() else {
                            return;
                        };
                        let state = Arc::clone(state.inner());
                        let app = app.clone();
                        tauri::async_runtime::spawn_blocking(move || {
                            if let Err(error) = open_broadcast_in_browser(&state) {
                                append_log(
                                    &state,
                                    &format!("Launcher menu browser open failed: {error}"),
                                );
                                show_error_dialog(&app, "Broadcast 网页打开失败", &error);
                            }
                        });
                    }
                    "restart-codex" => {
                        let Some(state) = app.try_state::<Arc<LauncherState>>() else {
                            return;
                        };
                        let state = Arc::clone(state.inner());
                        let app = app.clone();
                        tauri::async_runtime::spawn_blocking(move || {
                            if let Err(error) = restart_launcher(&app, &state) {
                                append_log(
                                    &state,
                                    &format!("Launcher menu restart failed: {error}"),
                                );
                                show_error_dialog(
                                    &app,
                                    "Broadcast 启动失败",
                                    &format!("{error}\n\n请确认官方 Codex/ChatGPT App 已安装。"),
                                );
                            }
                        });
                    }
                    "autostart" => {
                        let manager = app.autolaunch();
                        let previous = autostart_confirmed.load(Ordering::SeqCst);
                        let mut confirmed_before = previous;
                        let operation_error = match manager.is_enabled() {
                            Ok(enabled) => {
                                confirmed_before = enabled;
                                autostart_confirmed.store(enabled, Ordering::SeqCst);
                                let result = if enabled {
                                    manager.disable()
                                } else {
                                    manager.enable()
                                };
                                result.err().map(|error| error.to_string())
                            }
                            Err(error) => Some(error.to_string()),
                        };
                        let sync_error = match manager.is_enabled() {
                            Ok(enabled) => {
                                autostart_confirmed.store(enabled, Ordering::SeqCst);
                                autostart_menu.set_checked(enabled).unwrap();
                                None
                            }
                            Err(error) => {
                                autostart_menu.set_checked(confirmed_before).unwrap();
                                autostart_confirmed.store(confirmed_before, Ordering::SeqCst);
                                Some(error.to_string())
                            }
                        };
                        if let Some(error) = operation_error.or(sync_error) {
                            show_error_dialog(app, "Broadcast 自启动设置失败", &error);
                        }
                    }
                    "quit" => {
                        let Some(state) = app.try_state::<Arc<LauncherState>>() else {
                            return;
                        };
                        let lifecycle = state.lifecycle.lock().unwrap();
                        if state.update_in_progress.load(Ordering::SeqCst) {
                            return;
                        }
                        stop_service_locked(app, &state);
                        drop(lifecycle);
                        app.exit(0);
                    }
                    _ => {}
                })
                .build(app)?;

            let periodic_update_app = app.handle().clone();
            let periodic_update_state = Arc::clone(&state);
            let periodic_check_update = check_update.clone();
            let periodic_quit = quit.clone();
            thread::spawn(move || loop {
                thread::sleep(UPDATE_CHECK_INTERVAL);
                let app_handle = periodic_update_app.clone();
                let state = Arc::clone(&periodic_update_state);
                let check_update = periodic_check_update.clone();
                let quit = periodic_quit.clone();
                tauri::async_runtime::spawn(async move {
                    offer_update(&app_handle, &state, &check_update, &quit, false).await;
                });
            });

            let app_handle = app.handle().clone();
            let startup_check_update = check_update.clone();
            let startup_quit = quit.clone();
            tauri::async_runtime::spawn(async move {
                if let Some((legacy_skill, backup_path)) = legacy_skill_conflict {
                    match resolve_legacy_skill_conflict(&app_handle, &legacy_skill, &backup_path) {
                        Ok(true) => {}
                        Ok(false) => {
                            app_handle.exit(0);
                            return;
                        }
                        Err(error) => {
                            show_error_dialog(
                                &app_handle,
                                "Broadcast Skill 更新失败",
                                &format!("无法保留旧 Skill：{error}"),
                            );
                            app_handle.exit(1);
                            return;
                        }
                    }
                }
                if let Err(error) = start_launcher(&app_handle, &state) {
                    append_log(&state, &format!("Launcher startup failed: {error}"));
                    update_snapshot(&app_handle, &state, |snapshot| {
                        snapshot.phase = "error".into();
                        snapshot.message = error.clone();
                    });
                    show_error_dialog(
                        &app_handle,
                        "Broadcast 启动失败",
                        &format!(
                            "{error}\n\n请确认官方 Codex/ChatGPT App 已安装。详情见启动日志。"
                        ),
                    );
                }
                offer_update(
                    &app_handle,
                    &state,
                    &startup_check_update,
                    &startup_quit,
                    false,
                )
                .await;
            });
            Ok(())
        })
        .build(context)
        .expect("failed to build Broadcast");

    app.run(|app_handle, event| match event {
        #[cfg(target_os = "macos")]
        tauri::RunEvent::Reopen { .. } => {
            let Some(state) = app_handle.try_state::<Arc<LauncherState>>() else {
                return;
            };
            let result = start_launcher(app_handle, &state).map(|_| open_broadcast(&state));
            if let Err(error) = result {
                append_log(&state, &format!("Launcher panel reopen failed: {error}"));
                show_error_dialog(
                    app_handle,
                    "Broadcast 打开失败",
                    &format!("{error}\n\n请确认官方 Codex/ChatGPT App 已安装。"),
                );
            }
        }
        tauri::RunEvent::ExitRequested { code, api, .. } => {
            if let Some(state) = app_handle.try_state::<Arc<LauncherState>>() {
                let _lifecycle = state.lifecycle.lock().unwrap();
                if code != Some(tauri::RESTART_EXIT_CODE)
                    && state.update_in_progress.load(Ordering::SeqCst)
                {
                    api.prevent_exit();
                    return;
                }
                stop_service_locked(app_handle, &state);
            }
        }
        tauri::RunEvent::Exit => {
            if let Some(state) = app_handle.try_state::<Arc<LauncherState>>() {
                stop_service(app_handle, &state);
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                unsafe {
                    libc::flock(state._instance_lock.as_raw_fd(), libc::LOCK_UN);
                }
            }
        }
        _ => {}
    });
}

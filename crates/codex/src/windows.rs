//! The packaged Codex on Windows: its processes from WMI, launching it
//! through `IApplicationActivationManager`, stopping its process tree and raising its window.

use std::path::Path;
use std::process::Stdio;
use std::sync::LazyLock;

use anyhow::{Result, bail};
use regex::Regex;
use serde_json::Value;

use crate::process::Record;

/// `windowsHide`.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const PROCESSES: &str = r#"
$ErrorActionPreference = 'Stop'
$app = $env:BROADCAST_CODEX_APP_PATH
$name = [IO.Path]::GetFileName($app)
$processes = @(Get-CimInstance Win32_Process -Filter "Name = '$name'" |
  Where-Object { $_.ExecutablePath -eq $app } |
  Select-Object ProcessId, ParentProcessId, ExecutablePath, CommandLine)
[Console]::Out.Write(($processes | ConvertTo-Json -Compress))
"#;

const ACTIVATE_APP: &str = r#"
$ErrorActionPreference = 'Stop'
$source = @'
using System;
using System.Runtime.InteropServices;

[ComImport]
[Guid("2E941141-7F97-4756-BA1D-9DECDE894A3D")]
[InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IApplicationActivationManager
{
    [PreserveSig]
    int ActivateApplication(
        [MarshalAs(UnmanagedType.LPWStr)] string appUserModelId,
        [MarshalAs(UnmanagedType.LPWStr)] string arguments,
        uint options,
        out uint processId);
}

[ComImport]
[Guid("45BA127D-10A8-46EA-8AB7-56EA9078943C")]
class ApplicationActivationManager {}

public static class PackagedAppActivator
{
    public static uint Activate(string appUserModelId, string arguments)
    {
        var manager = (IApplicationActivationManager)new ApplicationActivationManager();
        try
        {
            uint processId;
            var result = manager.ActivateApplication(appUserModelId, arguments, 0, out processId);
            if (result < 0) Marshal.ThrowExceptionForHR(result);
            return processId;
        }
        finally
        {
            Marshal.FinalReleaseComObject(manager);
        }
    }
}
'@
Add-Type -TypeDefinition $source

$app = $env:BROADCAST_CODEX_APP_PATH
$profile = $env:BROADCAST_CODEX_PROFILE
$port = $env:BROADCAST_CODEX_PORT
$package = Get-AppxPackage | Where-Object {
  $app.StartsWith(($_.InstallLocation + [IO.Path]::DirectorySeparatorChar), [StringComparison]::OrdinalIgnoreCase)
} | Select-Object -First 1
if ($null -eq $package) { throw "Unable to find the Codex package for $app" }

$relativeExecutable = $app.Substring($package.InstallLocation.Length).TrimStart('\', '/').Replace('\', '/')
$manifest = $package | Get-AppxPackageManifest
$application = @($manifest.Package.Applications.Application) | Where-Object {
  ([string]$_.Executable).Replace('\', '/') -eq $relativeExecutable
} | Select-Object -First 1
if ($null -eq $application) { throw "Unable to find the Codex application manifest entry" }

$appUserModelId = $package.PackageFamilyName + '!' + $application.Id
$escapedProfile = $profile.Replace('"', '\"')
$arguments = '--user-data-dir="' + $escapedProfile + '"' +
  ' --remote-debugging-address=127.0.0.1' +
  ' --remote-debugging-port=' + $port +
  ' --remote-allow-origins=http://127.0.0.1:' + $port
$processId = [PackagedAppActivator]::Activate($appUserModelId, $arguments)
[Console]::Out.Write($processId)
"#;

// A package-wide AUMID cannot choose between the user's own Codex and the launcher's managed
// instance, so the window is raised by process ID instead.
const ACTIVATE_WINDOW: &str = r#"
$ErrorActionPreference = 'Stop'
$source = @'
using System;
using System.Runtime.InteropServices;

public static class CodexWindowActivator
{
    [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
}
'@
Add-Type -TypeDefinition $source

$processId = [int]$env:BROADCAST_CODEX_APP_PID
$process = Get-Process -Id $processId -ErrorAction SilentlyContinue
$handle = if ($process) { $process.MainWindowHandle } else { [IntPtr]::Zero }

if ($handle -eq [IntPtr]::Zero) {
  [Console]::Out.Write((ConvertTo-Json -Compress @{
    handle = 0
    minimized = $false
    foreground = $false
    reason = 'no-main-window'
  }))
  exit 0
}

# Only restore when minimized: an unconditional SW_RESTORE would unmaximize a
# maximized window. ShowWindow reports the previous visibility, not success.
if ([CodexWindowActivator]::IsIconic($handle)) {
  [void][CodexWindowActivator]::ShowWindow($handle, 9)
}
$foreground = [CodexWindowActivator]::SetForegroundWindow($handle)
$reason = 'foreground-denied'
if ($foreground) { $reason = '' }

[Console]::Out.Write((ConvertTo-Json -Compress @{
  handle = $handle.ToInt64()
  minimized = [CodexWindowActivator]::IsIconic($handle)
  foreground = $foreground
  reason = $reason
}))
"#;

static NOT_RUNNING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)not found|not running").expect("valid regex"));

/// `runWindowsPowerShell`: the script's trimmed stdout, or its stderr as the error.
async fn powershell(script: &str, environment: &[(&str, &str)]) -> Result<String> {
    let output = crate::command("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .envs(environment.iter().copied())
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        bail!(
            "{}",
            if stderr.is_empty() {
                "Windows PowerShell command failed".to_owned()
            } else {
                stderr
            }
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// `windowsCodexProcesses`: every process running the app's executable, with its parent.
pub async fn processes(app: &Path) -> Result<Vec<(Record, u32)>> {
    let app = app.to_string_lossy();
    let output = powershell(PROCESSES, &[("BROADCAST_CODEX_APP_PATH", &app)]).await?;
    if output.is_empty() {
        return Ok(Vec::new());
    }
    let items = match serde_json::from_str(&output)? {
        Value::Array(items) => items,
        item => vec![item],
    };
    items
        .into_iter()
        .map(|item| {
            let number = |field: &str| {
                item[field]
                    .as_u64()
                    .and_then(|value| u32::try_from(value).ok())
            };
            let (Some(pid), Some(parent)) = (number("ProcessId"), number("ParentProcessId")) else {
                bail!("Unexpected Win32_Process record: {item}");
            };
            let command = ["CommandLine", "ExecutablePath"]
                .into_iter()
                .filter_map(|field| item[field].as_str())
                .find(|value| !value.is_empty())
                .map_or_else(|| app.to_string(), str::to_owned);
            Ok((Record { pid, command }, parent))
        })
        .collect()
}

/// `managedCodexProcesses`: the root processes whose command line carries Broadcast's profile.
pub async fn managed(app: &Path, profile: &Path) -> Result<Vec<Record>> {
    let profile = std::path::absolute(profile)
        .unwrap_or_else(|_| profile.to_path_buf())
        .to_string_lossy()
        .to_lowercase();
    let managed: Vec<(Record, u32)> = processes(app)
        .await?
        .into_iter()
        .filter(|(record, _)| {
            let command = record.command.to_lowercase();
            command.contains("--user-data-dir") && command.contains(&profile)
        })
        .collect();
    let pids: Vec<u32> = managed.iter().map(|(record, _)| record.pid).collect();
    Ok(managed
        .into_iter()
        .filter(|(_, parent)| !pids.contains(parent))
        .map(|(record, _)| record)
        .collect())
}

/// `activateWindowsCodex`: start the packaged app with Broadcast's profile and debugging port.
pub async fn launch(app: &Path, profile: &Path, port: u16) -> Result<()> {
    let output = powershell(
        ACTIVATE_APP,
        &[
            ("BROADCAST_CODEX_APP_PATH", &app.to_string_lossy()),
            ("BROADCAST_CODEX_PROFILE", &profile.to_string_lossy()),
            ("BROADCAST_CODEX_PORT", &port.to_string()),
        ],
    )
    .await?;
    if !output.parse::<u32>().is_ok_and(|pid| pid > 0) {
        bail!("Packaged Codex activation returned an invalid process ID: {output}");
    }
    Ok(())
}

/// `stopWindowsCodex`: end the process tree.
pub async fn stop(pid: u32) -> Result<()> {
    let output = crate::command("taskkill.exe")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .output()
        .await?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() && !NOT_RUNNING.is_match(&stderr) {
        let stderr = stderr.trim();
        bail!(
            "{}",
            if stderr.is_empty() {
                format!("Unable to stop Codex process {pid}")
            } else {
                stderr.to_owned()
            }
        );
    }
    Ok(())
}

/// `activateWindowsCodexWindow`: restore the process's main window and bring it to the front.
pub async fn activate_window(pid: u32) -> Result<()> {
    let output = powershell(
        ACTIVATE_WINDOW,
        &[("BROADCAST_CODEX_APP_PID", &pid.to_string())],
    )
    .await?;
    let result: Value = serde_json::from_str(&output)?;
    if result["handle"] == 0 {
        bail!("Codex process {pid} has no main window to activate");
    }
    if result["foreground"] != true {
        bail!("Windows refused to foreground the Codex window (pid {pid})");
    }
    Ok(())
}

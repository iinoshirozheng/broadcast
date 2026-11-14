# Uninstall Broadcast on Windows

1. Quit Broadcast from its system-tray menu.
2. Open **Settings > Apps > Installed apps**.
3. Find **Broadcast**, open its menu, and select **Uninstall**.
4. Complete the NSIS uninstaller.

The uninstaller removes the application files. It keeps Broadcast issues,
attachments, settings, logs, the independent Codex profile, and the bundled
Skill so that a later installation can reuse them.

To remove that retained data, close Broadcast and delete these directories
for the current Windows user:

- `%APPDATA%\Broadcast`
- `%LOCALAPPDATA%\Broadcast`
- `%USERPROFILE%\.agents\skills\manage-broadcast`

Removing the first directory permanently deletes local Broadcast issues and
attachments. It does not remove the official Codex application, the user's
normal Codex profile, or Codex projects.

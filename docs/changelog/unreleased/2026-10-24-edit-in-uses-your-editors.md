## Updater and security

- **Edit In uses the editors you set up:** over MCP, the control channel and in actions, `photo.editIn`,
  `photo.openAsLayers` and `editIn.savePreset` no longer take a program (`app`, `args`, `control`), the Edit In menu
  commands no longer take `app`, and `ui.set` can't change the editors in Settings ▸ External Editing; a saved Edit In
  preset or the editor in Settings opens instead. The command line (`run` without `--connect`) and the Edit In Presets
  dialog still choose one. On Windows, the system's default editor is started through Explorer rather than the command
  prompt (`cmd`).

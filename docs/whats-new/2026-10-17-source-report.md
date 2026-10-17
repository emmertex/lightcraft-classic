### See how a raw is read before any edit
- `photo.sourceReport` says, for each chosen photo, whether the app develops its sensor data or shows the camera's
  embedded preview, where its starting colour comes from (the file's own matrices, the camera database, a fit to the
  camera JPEG, a camera profile, or the neutral fallback), where its tone curve comes from, and which
  lens correction applies by itself. It changes nothing, and is meant for comparing renders and for bug reports
  (`docs/camera-preview-colour.md`).

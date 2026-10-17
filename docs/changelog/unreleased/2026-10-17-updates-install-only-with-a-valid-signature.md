## Updater and security

- **Updates install only with a valid signature:** a release's `SHA256SUMS.txt` is signed with a key pinned in
  `brand.toml`, and the app installs a download only when that list is signed, lists the file with a matching
  SHA-256 and names the offered version. A build without a key checks for updates and links to the release page.
  Thanks @clydecbrown. Fork #179. (no PR)

### Updates are signed
- The app installs an update only when the release's checksum list carries a valid signature from the key built
  into it and lists the downloaded file. A release that fails any of these checks isn't installed, and the update
  window says why. A build without a key (a fork, a dev build) still tells you about new versions and opens the
  release page.

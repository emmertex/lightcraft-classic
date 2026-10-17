//! `cargo xtask update-key`: the key a release's `SHA256SUMS.txt` is signed with, so the in-app
//! updater (`crates/updater`, `signature.rs`) installs only what the maintainers released.
//!
//! - `new --out FILE` makes a key pair on the maintainer's machine: the private key goes to FILE
//!   (outside the repository, a new file, readable by its owner only) and the public key is printed
//!   for `brand.toml` (`[updates] public_keys`).
//! - `sign FOLDER [--key-file FILE]` writes `FOLDER/SHA256SUMS.txt.sig`: the ed25519 signature of the
//!   exact bytes of `FOLDER/SHA256SUMS.txt`, base64. The key comes from FILE or from the environment
//!   variable `UPDATE_SIGNING_KEY` (the release workflow's secret: the private key file's contents).
//! - `verify FOLDER` checks `SHA256SUMS.txt.sig` against the keys in `brand.toml` (`$BRAND_FILE` if
//!   set; any of them, as the app does), and every file the list names that is in FOLDER against its
//!   SHA-256.
//!
//! The format is the app's: the shared test vector below is also checked in `crates/updater`.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};

pub const SUMS_FILE: &str = "SHA256SUMS.txt";
pub const SIG_FILE: &str = "SHA256SUMS.txt.sig";
/// The environment variable `sign` reads the private key from.
pub const KEY_ENV: &str = "UPDATE_SIGNING_KEY";
/// How a private key file starts, so it can never be mistaken for (or pasted as) the public key.
const SECRET_PREFIX: &str = "update-signing-secret:";

const USAGE: &str = "usage: cargo xtask update-key new --out FILE | sign FOLDER [--key-file FILE] | verify FOLDER";

pub fn run(root: &Path, args: &[&str]) -> Result<(), String> {
    match args {
        ["new", "--out", out] => {
            let public = new_key(root, Path::new(out))?;
            println!("private key written to {out} (keep it safe: a password manager, and the release environment's {KEY_ENV} secret)");
            println!("public key, for brand.toml:\n\n[updates]\npublic_keys = [\"{public}\"]");
            Ok(())
        }
        ["sign", folder, rest @ ..] => {
            let secret = match rest {
                ["--key-file", file] => std::fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?,
                [] => std::env::var(KEY_ENV).map_err(|_| format!("no key: set {KEY_ENV} or pass --key-file FILE"))?,
                _ => return Err(USAGE.into()),
            };
            let public = sign_folder(Path::new(folder), &secret)?;
            println!("{folder}/{SIG_FILE} written (public key {public})");
            Ok(())
        }
        ["verify", folder] => {
            let brand = crate::brand::Brand::current()?;
            let keys = brand.list("updates", "public_keys");
            let report = verify_folder(Path::new(folder), &keys)?;
            println!("{SIG_FILE} verified against brand.toml's update key ({report})");
            Ok(())
        }
        _ => Err(USAGE.into()),
    }
}

/// Make a key pair: the private key into `out`, the public key returned as `brand.toml` spells it.
pub fn new_key(root: &Path, out: &Path) -> Result<String, String> {
    if out.as_os_str().is_empty() {
        return Err("--out names no file".into());
    }
    if std::fs::symlink_metadata(out).is_ok() {
        return Err(format!("{} already exists: a key file is never overwritten", out.display()));
    }
    let parent = match out.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let parent = parent.canonicalize().map_err(|e| format!("{}: {e} (the folder must exist)", parent.display()))?;
    let root = root.canonicalize().map_err(|e| format!("{}: {e}", root.display()))?;
    if parent.starts_with(&root) {
        return Err(format!("{} is inside the repository: write the private key somewhere it can never be committed", out.display()));
    }
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| format!("no randomness from the system: {e}"))?;
    let signing = SigningKey::from_bytes(&seed);
    seed.fill(0);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let text = format!("{SECRET_PREFIX}{}\n", STANDARD.encode(signing.to_bytes()));
    file.write_all(text.as_bytes()).and_then(|()| file.sync_all()).map_err(|e| format!("{}: {e}", out.display()))?;
    Ok(public_text(&signing))
}

fn public_text(signing: &SigningKey) -> String {
    STANDARD.encode(signing.verifying_key().as_bytes())
}

/// The private key in `text` (a key file's contents).
fn parse_secret(text: &str) -> Result<SigningKey, String> {
    let body = text.trim().strip_prefix(SECRET_PREFIX).ok_or_else(|| format!("not a private key file (it starts with `{SECRET_PREFIX}`)"))?;
    let bytes = STANDARD.decode(body.trim()).map_err(|_| "the private key is not base64".to_string())?;
    let seed: [u8; 32] = bytes.as_slice().try_into().map_err(|_| "the private key is not 32 bytes".to_string())?;
    Ok(SigningKey::from_bytes(&seed))
}

/// The public keys in `texts` (`brand.toml`'s `public_keys`): all of them, or an error.
fn parse_public(texts: &[String]) -> Result<Vec<VerifyingKey>, String> {
    if texts.is_empty() {
        return Err("brand.toml has no [updates] public_keys: add the one `update-key new` printed".into());
    }
    let mut keys = Vec::new();
    for text in texts {
        let text = text.trim();
        if text.contains(SECRET_PREFIX.trim_end_matches(':')) {
            return Err("brand.toml's public_keys holds a PRIVATE key: remove it now, and make a new key pair".into());
        }
        let bytes = STANDARD.decode(text).map_err(|_| format!("brand.toml's public key `{text}` is not base64"))?;
        let bytes: [u8; 32] = bytes.as_slice().try_into().map_err(|_| format!("brand.toml's public key `{text}` is not 32 bytes"))?;
        keys.push(VerifyingKey::from_bytes(&bytes).map_err(|_| format!("brand.toml's public key `{text}` is not an ed25519 public key"))?);
    }
    Ok(keys)
}

/// Sign `folder/SHA256SUMS.txt` into `folder/SHA256SUMS.txt.sig`; returns the public key.
pub fn sign_folder(folder: &Path, secret: &str) -> Result<String, String> {
    let signing = parse_secret(secret)?;
    let list_path = folder.join(SUMS_FILE);
    let list = std::fs::read(&list_path).map_err(|e| format!("{}: {e}", list_path.display()))?;
    if list.is_empty() {
        return Err(format!("{} is empty", list_path.display()));
    }
    let sig = STANDARD.encode(signing.sign(&list).to_bytes());
    let sig_path = folder.join(SIG_FILE);
    std::fs::write(&sig_path, format!("{sig}\n")).map_err(|e| format!("{}: {e}", sig_path.display()))?;
    Ok(public_text(&signing))
}

/// Check `folder`'s list and signature against `public` (any of them), and each listed file present
/// in `folder` against its SHA-256. Returns a one-line summary.
pub fn verify_folder(folder: &Path, public: &[String]) -> Result<String, String> {
    let keys = parse_public(public)?;
    let list_path = folder.join(SUMS_FILE);
    let list = std::fs::read(&list_path).map_err(|e| format!("{}: {e}", list_path.display()))?;
    let sig_path = folder.join(SIG_FILE);
    let sig = std::fs::read_to_string(&sig_path).map_err(|e| format!("{}: {e} (the list is not signed)", sig_path.display()))?;
    let raw = STANDARD.decode(sig.trim()).map_err(|_| format!("{SIG_FILE} is not base64"))?;
    let raw: [u8; 64] = raw.as_slice().try_into().map_err(|_| format!("{SIG_FILE} is not an ed25519 signature"))?;
    let signature = Signature::from_bytes(&raw);
    if !keys.iter().any(|key| key.verify_strict(&list, &signature).is_ok()) {
        return Err(format!("{SIG_FILE} does not match: {SUMS_FILE} was changed, or another key signed it"));
    }
    let text = String::from_utf8_lossy(&list);
    let (mut listed, mut checked, mut bad) = (0usize, 0usize, Vec::new());
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let Some((sha, name)) = line.split_once(char::is_whitespace) else { continue };
        let name = name.trim_start();
        let name = name.strip_prefix('*').unwrap_or(name).trim();
        listed += 1;
        let path = folder.join(name);
        // only plain names: a list never sends the check outside the folder
        if name.contains(['/', '\\']) || name == ".." || !path.is_file() {
            continue;
        }
        checked += 1;
        if !sha256_file(&path)?.eq_ignore_ascii_case(sha) {
            bad.push(name.to_string());
        }
    }
    if !bad.is_empty() {
        return Err(format!("signature good, but these files do not match the list: {}", bad.join(", ")));
    }
    Ok(format!("{listed} files listed, {checked} present here and matching"))
}

fn sha256_file(path: &Path) -> Result<String, String> {
    use sha2::{Digest as _, Sha256};
    use std::io::Read as _;
    let mut file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf).map_err(|e| format!("{}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xtask-update-key-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("scratch");
        d
    }

    /// A throwaway key for tests, never a release key.
    fn test_secret(seed: u8) -> String {
        format!("{SECRET_PREFIX}{}\n", STANDARD.encode([seed; 32]))
    }

    fn release(dir: &Path, files: &[(&str, &[u8])]) {
        let mut list = String::new();
        for (name, body) in files {
            std::fs::write(dir.join(name), body).expect("file");
            use sha2::Digest as _;
            let sha: String = sha2::Sha256::digest(body).iter().map(|b| format!("{b:02x}")).collect();
            list.push_str(&format!("{sha}  {name}\n"));
        }
        std::fs::write(dir.join(SUMS_FILE), list).expect("list");
    }

    #[test]
    fn a_new_key_is_written_once_outside_the_repository_for_its_owner_only() {
        let dir = scratch("new");
        let out = dir.join("update.key");
        let public = new_key(&crate::root(), &out).expect("a key");
        let text = std::fs::read_to_string(&out).expect("read");
        assert!(text.starts_with(SECRET_PREFIX), "the file says what it is");
        assert!(!text.contains(&public), "the file is the private key");
        assert_eq!(public_text(&parse_secret(&text).expect("parses")), public, "the printed key is its public half");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&out).expect("meta").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "readable by its owner only");
        }
        // an existing file is never overwritten
        let err = new_key(&crate::root(), &out).expect_err("exists");
        assert!(err.contains("already exists"), "{err}");
        assert_eq!(std::fs::read_to_string(&out).expect("read"), text, "the first key is untouched");
        // a second key is a different key
        let other = new_key(&crate::root(), &dir.join("other.key")).expect("another");
        assert_ne!(other, public);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_key_is_never_written_into_the_repository() {
        /// Deletes the stand-in repository however the test ends.
        struct Gone(PathBuf);
        impl Drop for Gone {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        // a stand-in repository in the temp folder, not this checkout: should the check ever
        // regress, the key it writes lands here and is deleted, never left in the real repository
        let repo = Gone(scratch("repo"));
        let root = &repo.0;
        std::fs::create_dir_all(root.join("xtask")).expect("subfolder");
        for out in [root.join("update-key-test.secret"), root.join("xtask").join("update-key-test.secret")] {
            let err = new_key(root, &out).expect_err("inside the repository");
            assert!(err.contains("inside the repository"), "{err}");
            assert!(!out.exists(), "nothing was written");
        }
        // a relative path resolves from the working folder: the same check holds through `..`
        let sneaky = root.join("xtask").join("..").join("update-key-test.secret");
        assert!(new_key(root, &sneaky).expect_err("inside, by another spelling").contains("inside the repository"));
        let err = new_key(root, &std::env::temp_dir().join("no-such-folder-xtask").join("k")).expect_err("no folder");
        assert!(err.contains("must exist"), "{err}");
    }

    #[test]
    fn a_signed_folder_verifies_and_a_changed_one_does_not() {
        let dir = scratch("sign");
        release(&dir, &[("app-1.0.0-windows-x64.msi", b"installer"), ("app-1.0.0-macos-universal.dmg", b"image")]);
        let public = sign_folder(&dir, &test_secret(1)).expect("signed");
        let summary = verify_folder(&dir, std::slice::from_ref(&public)).expect("verifies");
        assert!(summary.contains("2 files listed, 2 present"), "{summary}");
        // a folder with only the list and its signature (the release job's) verifies too
        std::fs::remove_file(dir.join("app-1.0.0-macos-universal.dmg")).expect("rm");
        assert!(verify_folder(&dir, std::slice::from_ref(&public)).expect("verifies").contains("1 present"));

        // a file that is not the one listed
        std::fs::write(dir.join("app-1.0.0-windows-x64.msi"), b"swapped").expect("swap");
        let err = verify_folder(&dir, std::slice::from_ref(&public)).expect_err("a swapped file");
        assert!(err.contains("app-1.0.0-windows-x64.msi"), "{err}");
        std::fs::write(dir.join("app-1.0.0-windows-x64.msi"), b"installer").expect("restore");

        // a signature by another key: what a mismatch with brand.toml looks like
        let other = sign_folder(&dir, &test_secret(2)).expect("signed by another key");
        let err = verify_folder(&dir, std::slice::from_ref(&public)).expect_err("another key");
        assert!(err.contains("does not match"), "{err}");
        verify_folder(&dir, std::slice::from_ref(&other)).expect("its own key verifies");
        // while the key is rotated, brand.toml holds both, and either signature verifies
        verify_folder(&dir, &[public.clone(), other.clone()]).expect("the new key of two");

        // a changed list
        sign_folder(&dir, &test_secret(1)).expect("signed");
        let mut list = std::fs::read_to_string(dir.join(SUMS_FILE)).expect("list");
        list.push_str(&format!("{}  extra.msi\n", "0".repeat(64)));
        std::fs::write(dir.join(SUMS_FILE), list).expect("tamper");
        let err = verify_folder(&dir, std::slice::from_ref(&public)).expect_err("a changed list");
        assert!(err.contains("does not match"), "{err}");

        // no signature
        std::fs::remove_file(dir.join(SIG_FILE)).expect("rm");
        let err = verify_folder(&dir, std::slice::from_ref(&public)).expect_err("unsigned");
        assert!(err.contains("not signed"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keys_in_the_wrong_place_or_shape_are_refused() {
        let dir = scratch("shape");
        release(&dir, &[("a.msi", b"a")]);
        let secret = test_secret(3);
        // the private key where the public one belongs
        let err = verify_folder(&dir, &[secret.trim().to_string()]).expect_err("a private key in brand.toml");
        assert!(err.contains("PRIVATE"), "{err}");
        assert!(verify_folder(&dir, &[]).expect_err("no key").contains("no [updates] public_keys"));
        assert!(verify_folder(&dir, &["AAAA".to_string()]).is_err());
        // a public key is not a private key file
        let public = public_text(&parse_secret(&secret).expect("secret"));
        assert!(sign_folder(&dir, &public).is_err(), "signing needs the private key file");
        assert!(sign_folder(&dir, "update-signing-secret:AAAA").is_err());
        assert!(!dir.join(SIG_FILE).exists(), "nothing was signed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same vector is checked in `crates/updater/src/signature.rs`: the tool and the app agree.
    #[test]
    fn the_shared_test_vector_matches_the_app() {
        // throwaway test key: seed [42; 32], never a release key
        let signing = parse_secret(&test_secret(42)).expect("secret");
        assert_eq!(public_text(&signing), "GX9rI+FshTLGq8g4+s1ep4m+DHaykgM0A5v6iz02jWE=");
        let list = "0000000000000000000000000000000000000000000000000000000000000000  app.msi\n";
        assert_eq!(
            STANDARD.encode(signing.sign(list.as_bytes()).to_bytes()),
            "r1UAqfvohrdWdsN4DSyKUrEiI+g6DU8WgeI9CwblTMcuEqOXaZ/JPHc7AsL0UXUMdNspO1Kr0ToCYDCUWFINAA=="
        );
    }
}

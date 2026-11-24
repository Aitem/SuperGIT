//! Getting a release onto this computer: this platform's package, checked
//! against the release's `SHA256SUMS`, saved to Downloads or installed over
//! the running copy. The running app keeps its old code until it restarts.
//!
//! Downloads go through `curl`, which (unlike a browser) does not mark files
//! as quarantined: the installed app opens without a Gatekeeper prompt.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use sg_github::{Asset, Release};
use sha2::{Digest as _, Sha256};

/// This platform's package, as `.github/workflows/release.yml` names it.
fn package_name() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("supergit-macos-arm64.zip"),
        ("linux", "x86_64") => Some("supergit-linux-x86_64.tar.gz"),
        ("linux", "aarch64") => Some("supergit-linux-arm64.tar.gz"),
        _ => None,
    }
}

/// This platform's package in `release`.
pub fn package(release: &Release) -> Option<&Asset> {
    let name = package_name()?;
    release.assets.iter().find(|asset| asset.name == name)
}

/// What an install replaces: the `.app` bundle on macOS, the binary
/// elsewhere. `None` for builds from source (in a Cargo target directory)
/// and for a macOS binary outside a bundle.
pub fn install_target() -> Option<&'static Path> {
    static TARGET: OnceLock<Option<PathBuf>> = OnceLock::new();
    TARGET
        .get_or_init(|| {
            let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
            target_for(&exe, cfg!(target_os = "macos"))
        })
        .as_deref()
}

fn target_for(exe: &Path, macos: bool) -> Option<PathBuf> {
    // Cargo marks its target directories with `CACHEDIR.TAG`.
    let from_source = exe.ancestors().skip(1).any(|dir| {
        dir.file_name().is_some_and(|name| name == "target") && dir.join("CACHEDIR.TAG").is_file()
    });
    if from_source {
        return None;
    }
    if !macos {
        return Some(exe.to_path_buf());
    }
    let bin_dir = exe.parent()?;
    let bundle = bin_dir.parent()?.parent()?;
    (bin_dir.file_name()? == "MacOS" && bundle.extension()? == "app").then(|| bundle.to_path_buf())
}

/// The user's Downloads folder: on Linux `XDG_DOWNLOAD_DIR` from
/// `user-dirs.dirs` when set, else `~/Downloads`.
pub fn downloads_dir() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    if !cfg!(target_os = "macos")
        && let Some(config) = crate::settings::xdg_dir("XDG_CONFIG_HOME", ".config")
        && let Ok(text) = std::fs::read_to_string(config.join("user-dirs.dirs"))
        && let Some(dir) = xdg_download_dir(&text, &home)
    {
        return Some(dir);
    }
    Some(home.join("Downloads"))
}

/// `XDG_DOWNLOAD_DIR="$HOME/Downloads"` → `/home/me/Downloads`.
fn xdg_download_dir(text: &str, home: &Path) -> Option<PathBuf> {
    let value = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("XDG_DOWNLOAD_DIR="))?
        .trim()
        .trim_matches('"');
    let path = match value.strip_prefix("$HOME") {
        Some(rest) => home.join(rest.trim_start_matches('/')),
        None => PathBuf::from(value),
    };
    path.is_absolute().then_some(path)
}

/// Save this platform's package of `release` in `dir`, named with its
/// version (`supergit-v0.4.0-macos-arm64.zip`). `progress` counts bytes.
pub fn download(release: &Release, dir: &Path, progress: &AtomicU64) -> Result<PathBuf> {
    let (asset, sha256) = checked_package(release)?;
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let file = dir.join(versioned_name(&asset.name, &release.tag));
    let part = dir.join(format!(
        "{}.part",
        file.file_name()
            .map_or_else(Default::default, |n| n.to_string_lossy())
    ));
    let fetched = fetch(&asset.url, &part, progress).and_then(|()| verify(&part, &sha256));
    if fetched.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    fetched?;
    std::fs::rename(&part, &file)?;
    Ok(file)
}

/// Replace `target` (`install_target`) with `release`'s package. The new
/// copy must start on this machine first; on any failure `target` stays.
pub fn install(release: &Release, target: &Path, progress: &AtomicU64) -> Result<()> {
    let (asset, sha256) = checked_package(release)?;
    let parent = target
        .parent()
        .with_context(|| format!("{} has no parent folder", target.display()))?;
    // Next to the target: the same file system, so the swap is a rename.
    const STAGING: &str = ".supergit-update-";
    // Leftovers of installs cut short (SuperGIT quit mid-download).
    for entry in std::fs::read_dir(parent).into_iter().flatten().flatten() {
        if entry.file_name().to_string_lossy().starts_with(STAGING) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
    let staging = parent.join(format!("{STAGING}{}", std::process::id()));
    std::fs::create_dir(&staging).with_context(|| {
        format!(
            "cannot write to {}: download the update instead",
            parent.display()
        )
    })?;
    let result = install_staged(asset, &sha256, target, &staging, progress);
    let _ = std::fs::remove_dir_all(&staging);
    result
}

fn install_staged(
    asset: &Asset,
    sha256: &str,
    target: &Path,
    staging: &Path,
    progress: &AtomicU64,
) -> Result<()> {
    let archive = staging.join(&asset.name);
    fetch(&asset.url, &archive, progress)?;
    verify(&archive, sha256)?;
    let unpacked = staging.join("unpacked");
    std::fs::create_dir(&unpacked)?;
    let (new, exe) = if asset.name.ends_with(".zip") {
        run(Command::new("ditto")
            .args(["-x", "-k"])
            .arg(&archive)
            .arg(&unpacked))?;
        let app = unpacked.join("SuperGIT.app");
        let exe = app.join("Contents/MacOS/supergit");
        (app, exe)
    } else {
        run(Command::new("tar")
            .arg("-xzf")
            .arg(&archive)
            .arg("-C")
            .arg(&unpacked))?;
        let exe = unpacked.join("supergit");
        (exe.clone(), exe)
    };
    if !exe.is_file() {
        bail!("{} holds no SuperGIT", asset.name);
    }
    // Architecture and libraries: it must run here before it replaces a
    // working copy. `TERM` skips the login-shell import at startup.
    run(Command::new(&exe).arg("--help").env("TERM", "dumb"))
        .context("the downloaded SuperGIT does not start")?;
    replace(&new, target, &staging.join("previous"))
}

/// Put `new` where `target` is. A file is replaced in one rename; a bundle
/// moves aside first and comes back when the second rename fails.
fn replace(new: &Path, target: &Path, aside: &Path) -> Result<()> {
    let failed = || format!("cannot replace {}", target.display());
    if !new.is_dir() {
        return std::fs::rename(new, target).with_context(failed);
    }
    std::fs::rename(target, aside).with_context(failed)?;
    if let Err(error) = std::fs::rename(new, target) {
        let _ = std::fs::rename(aside, target);
        return Err(error).with_context(failed);
    }
    Ok(())
}

/// Start `target` again once this process has exited; the caller quits
/// right after. macOS opens the bundle like a Dock click; elsewhere the
/// binary runs from `/`, so both reopen the last repository.
pub fn relaunch_after_exit(target: &Path) -> Result<()> {
    let mut program = Vec::new();
    if cfg!(target_os = "macos") {
        program.push(PathBuf::from("open"));
    }
    program.push(target.to_path_buf());
    relaunch_command(std::process::id(), &program)
        .spawn()
        .context("cannot restart SuperGIT")?;
    Ok(())
}

/// `sh` waits for `pid` to exit (30 s at most), then runs `program`.
fn relaunch_command(pid: u32, program: &[PathBuf]) -> Command {
    use std::os::unix::process::CommandExt as _;
    const SCRIPT: &str = r#"i=0
while kill -0 "$0" 2>/dev/null && [ "$i" -lt 300 ]; do sleep 0.1; i=$((i + 1)); done
exec "$@""#;
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(SCRIPT)
        .arg(pid.to_string())
        .args(program)
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Out of the launching terminal's job control (Ctrl-C there).
        .process_group(0);
    command
}

/// The package and its SHA-256 from the release's `SHA256SUMS`.
fn checked_package(release: &Release) -> Result<(&Asset, String)> {
    let asset = package(release).with_context(|| {
        format!(
            "{} has no package for {} {}",
            release.tag,
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let sums = release
        .assets
        .iter()
        .find(|asset| asset.name == "SHA256SUMS")
        .with_context(|| format!("{} has no SHA256SUMS", release.tag))?;
    let sums = fetch_text(&sums.url)?;
    let sha256 = checksum_for(&sums, &asset.name)
        .with_context(|| format!("SHA256SUMS of {} lists no {}", release.tag, asset.name))?;
    Ok((asset, sha256))
}

/// The digest `sha256sum` lists for `name`.
fn checksum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (digest, file) = line.split_once(char::is_whitespace)?;
        // `*` marks binary mode.
        let file = file.trim_start().trim_start_matches('*');
        (file == name && digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| digest.to_ascii_lowercase())
    })
}

fn verify(file: &Path, sha256: &str) -> Result<()> {
    let mut hasher = Sha256::new();
    let mut input =
        std::fs::File::open(file).with_context(|| format!("cannot read {}", file.display()))?;
    std::io::copy(&mut input, &mut hasher)?;
    let actual: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if actual != sha256 {
        bail!("the download does not match SHA256SUMS (got {actual})");
    }
    Ok(())
}

/// `supergit-macos-arm64.zip` of `v0.4.0` → `supergit-v0.4.0-macos-arm64.zip`.
fn versioned_name(name: &str, tag: &str) -> String {
    match name.strip_prefix("supergit-") {
        Some(rest) => format!("supergit-{tag}-{rest}"),
        None => format!("{tag}-{name}"),
    }
}

/// Run a command to completion; its stderr is the error.
fn run(command: &mut Command) -> Result<()> {
    let out = command
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("cannot run {:?}", command.get_program()))?;
    if !out.status.success() {
        bail!(
            "{:?} failed: {}",
            command.get_program(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn curl() -> Command {
    let mut command = Command::new("curl");
    command.args([
        "--silent",
        "--show-error",
        "--fail",
        "--location",
        "--connect-timeout",
        "30",
        // Give up on a stalled transfer, not on a slow one.
        "--speed-limit",
        "1024",
        "--speed-time",
        "60",
    ]);
    command.stdin(Stdio::null());
    command
}

fn fetch_text(url: &str) -> Result<String> {
    let out = curl().arg(url).output().context("cannot run curl")?;
    if !out.status.success() {
        bail!(
            "cannot download {url}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Download `url` to `path`, reporting its size in `progress` as it grows.
fn fetch(url: &str, path: &Path, progress: &AtomicU64) -> Result<()> {
    let mut child = curl()
        .arg("--output")
        .arg(path)
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot run curl")?;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if let Ok(meta) = std::fs::metadata(path) {
            progress.store(meta.len(), Ordering::Relaxed);
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    if !status.success() {
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        bail!("cannot download {url}: {}", stderr.trim());
    }
    if let Ok(meta) = std::fs::metadata(path) {
        progress.store(meta.len(), Ordering::Relaxed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("supergit_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn installs_over_bundles_and_binaries_but_not_cargo_builds() {
        let dir = temp_dir("install_target");
        let app = dir.join("Applications/SuperGIT.app");
        assert_eq!(
            target_for(&app.join("Contents/MacOS/supergit"), true),
            Some(app)
        );
        assert_eq!(target_for(&dir.join("bin/supergit"), true), None);
        assert_eq!(
            target_for(&dir.join("bin/supergit"), false),
            Some(dir.join("bin/supergit"))
        );
        let target = dir.join("repo/target");
        std::fs::create_dir_all(target.join("debug")).unwrap();
        std::fs::write(
            target.join("CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55",
        )
        .unwrap();
        assert_eq!(target_for(&target.join("debug/supergit"), false), None);
        let bundle = target.join("release/bundle.noindex/SuperGIT.app");
        assert_eq!(
            target_for(&bundle.join("Contents/MacOS/supergit"), true),
            None
        );
        // A folder that only happens to be called `target`.
        assert!(target_for(&dir.join("target/bin/supergit"), false).is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reads_the_download_folder_from_user_dirs() {
        let home = Path::new("/home/me");
        let text =
            "# comment\nXDG_DESKTOP_DIR=\"$HOME/Desktop\"\nXDG_DOWNLOAD_DIR=\"$HOME/Загрузки\"\n";
        assert_eq!(
            xdg_download_dir(text, home),
            Some(PathBuf::from("/home/me/Загрузки"))
        );
        assert_eq!(
            xdg_download_dir("XDG_DOWNLOAD_DIR=\"/data/dl\"", home),
            Some(PathBuf::from("/data/dl"))
        );
        assert_eq!(xdg_download_dir("XDG_DOWNLOAD_DIR=\"dl\"", home), None);
        assert_eq!(xdg_download_dir("", home), None);
    }

    #[test]
    fn finds_checksums_and_names_downloads_by_version() {
        let digest = "5a57ec31722f7faf8f16f60a455449cddc50cde1642d53eb1dcd3ccb41e799a8";
        let sums = format!(
            "{digest}  supergit-macos-arm64.zip\n{}  supergit-linux-arm64.tar.gz\n",
            "4".repeat(64)
        );
        assert_eq!(
            checksum_for(&sums, "supergit-macos-arm64.zip").as_deref(),
            Some(digest)
        );
        assert_eq!(
            checksum_for(
                &format!("{digest} *supergit-macos-arm64.zip"),
                "supergit-macos-arm64.zip"
            )
            .as_deref(),
            Some(digest)
        );
        assert_eq!(checksum_for(&sums, "supergit-linux-x86_64.tar.gz"), None);
        assert_eq!(
            checksum_for("abc  supergit-macos-arm64.zip", "supergit-macos-arm64.zip"),
            None
        );
        assert_eq!(
            versioned_name("supergit-macos-arm64.zip", "v0.4.0"),
            "supergit-v0.4.0-macos-arm64.zip"
        );
        assert_eq!(versioned_name("other.zip", "v0.4.0"), "v0.4.0-other.zip");
    }

    #[test]
    fn verifies_files_against_their_digest() {
        let dir = temp_dir("verify");
        let file = dir.join("package");
        std::fs::write(&file, "abc").unwrap();
        let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        verify(&file, abc).unwrap();
        assert!(verify(&file, &"0".repeat(64)).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn replaces_bundles_and_binaries_and_restores_a_bundle_on_failure() {
        let dir = temp_dir("replace");
        let target = dir.join("SuperGIT.app");
        let new = dir.join("staging/SuperGIT.app");
        std::fs::create_dir_all(target.join("Contents")).unwrap();
        std::fs::write(target.join("Contents/version"), "old").unwrap();
        std::fs::create_dir_all(new.join("Contents")).unwrap();
        std::fs::write(new.join("Contents/version"), "new").unwrap();
        replace(&new, &target, &dir.join("staging/previous")).unwrap();
        assert_eq!(
            std::fs::read_to_string(target.join("Contents/version")).unwrap(),
            "new"
        );
        // The new bundle cannot move (its folder is read-only): the old
        // one comes back.
        use std::os::unix::fs::PermissionsExt as _;
        let locked = dir.join("locked");
        std::fs::create_dir_all(locked.join("SuperGIT.app")).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
        let aside = dir.join("aside");
        assert!(replace(&locked.join("SuperGIT.app"), &target, &aside).is_err());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            std::fs::read_to_string(target.join("Contents/version")).unwrap(),
            "new"
        );
        assert!(!aside.exists());
        // A binary is replaced in one rename.
        let bin = dir.join("supergit");
        std::fs::write(&bin, "old").unwrap();
        std::fs::write(dir.join("supergit.new"), "new").unwrap();
        replace(&dir.join("supergit.new"), &bin, &dir.join("unused")).unwrap();
        assert_eq!(std::fs::read_to_string(&bin).unwrap(), "new");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn relaunches_only_after_the_process_exits() {
        let dir = temp_dir("relaunch");
        let marker = dir.join("started");
        let mut running = Command::new("sleep").arg("0.5").spawn().unwrap();
        let mut waiter = relaunch_command(running.id(), &[PathBuf::from("touch"), marker.clone()])
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(!marker.exists());
        running.wait().unwrap();
        waiter.wait().unwrap();
        assert!(marker.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Against GitHub: downloads the latest package and installs it over a
    /// stand-in for this platform's install target.
    /// `cargo test -p supergit live_install -- --ignored`
    #[test]
    #[ignore = "network"]
    fn live_install() {
        let release = sg_github::latest_release("Aitem/SuperGIT").unwrap();
        let dir = temp_dir("live_install");
        let progress = AtomicU64::new(0);
        let file = download(&release, &dir.join("Downloads"), &progress).unwrap();
        assert!(file.ends_with(versioned_name(
            &package(&release).unwrap().name,
            &release.tag
        )));
        assert_eq!(
            progress.load(Ordering::Relaxed),
            std::fs::metadata(&file).unwrap().len()
        );
        let target = if cfg!(target_os = "macos") {
            let app = dir.join("SuperGIT.app");
            std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
            app
        } else {
            dir.join("supergit")
        };
        std::fs::write(dir.join("placeholder"), "old").unwrap();
        if !cfg!(target_os = "macos") {
            std::fs::write(&target, "old").unwrap();
        }
        install(&release, &target, &AtomicU64::new(0)).unwrap();
        let exe = if cfg!(target_os = "macos") {
            target.join("Contents/MacOS/supergit")
        } else {
            target.clone()
        };
        let help = Command::new(&exe)
            .arg("--help")
            .env("TERM", "dumb")
            .output()
            .unwrap();
        assert!(help.status.success());
        // Only the target changed: no staging folder is left behind.
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        let target_name = target.file_name().unwrap().to_string_lossy().into_owned();
        let mut expected = vec![
            "Downloads".to_string(),
            "placeholder".to_string(),
            target_name,
        ];
        expected.sort();
        assert_eq!(names, expected);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

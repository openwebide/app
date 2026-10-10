//! Compile pinned source without executing package build code in the host context.
use anyhow::{Context, Result, bail};
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub const TOOLCHAIN: &str = "1.98.1";
pub const TARGET: &str = "wasm32-wasip2";
const SDK_MANIFEST: &str = include_str!("../../plugin-sdk/Cargo.toml");
const SDK_SOURCE: &str = include_str!("../../plugin-sdk/src/lib.rs");
const SDK_WIT: &str = include_str!("../../plugin-sdk/wit/plugin.wit");

pub fn sdk_digest() -> String {
    use sha2::{Digest, Sha256};
    let bytes = Sha256::digest(format!("{SDK_MANIFEST}\n{SDK_SOURCE}\n{SDK_WIT}"));
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The source directory must already be a verified immutable package snapshot.
/// Outputs are kept separate from the snapshot and published only after validation.
pub fn compile(source: &Path, output_name: &str, staging: &Path) -> Result<Vec<u8>> {
    if output_name.is_empty()
        || !output_name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        bail!("Invalid Rust library name");
    }
    if !source.join("Cargo.lock").is_file() || !source.join("Cargo.toml").is_file() {
        bail!("Rust plugins require Cargo.toml and a committed Cargo.lock");
    }
    let staging = staging.canonicalize()?;
    let source = source.canonicalize()?;
    let sdk = staging.join("sdk");
    for (path, contents) in [
        ("Cargo.toml", SDK_MANIFEST),
        ("src/lib.rs", SDK_SOURCE),
        ("wit/plugin.wit", SDK_WIT),
    ] {
        let path = sdk.join(path);
        std::fs::create_dir_all(path.parent().expect("SDK parent"))?;
        std::fs::write(path, contents.replace("\n[lints]\nworkspace = true\n", ""))?;
    }
    let cargo_home = staging.join("cargo-home");
    std::fs::create_dir_all(&cargo_home)?;
    let rustup_home = std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".rustup")))
        .context("Configure the host Rust toolchain")?
        .canonicalize()?;
    let cargo = tool(&rustup_home, "cargo")?;
    let rustc = tool(&rustup_home, "rustc")?;
    let patch = format!(
        "patch.crates-io.openwebide-plugin-sdk.path={:?}",
        sdk.to_string_lossy()
    );
    let common = ["--locked", "--manifest-path"];
    // Fetch performs no package build-script execution. Cargo config is read from
    // this trusted staging cwd, not from the plugin's working directory. Fetch
    // the complete lockfile: filtering to the WASI target can omit native
    // dependencies needed by proc macros and build scripts during cross builds.
    let mut fetch = command(&cargo, &rustc, &rustup_home, &cargo_home, &staging);
    fetch
        .arg("fetch")
        .args(common)
        .arg(source.join("Cargo.toml"))
        .args(["--config", &patch]);
    run(fetch, Duration::from_secs(180)).context("Fetch Rust plugin dependencies")?;
    let mut build = sandbox(&cargo, &source, &staging, &rustup_home)?;
    configure(&mut build, &rustc, &rustup_home, &cargo_home, &staging);
    build
        .args(["build", "--release", "--offline"])
        .args(common)
        .arg(source.join("Cargo.toml"))
        .args(["--target", TARGET, "--config", &patch]);
    run(build, Duration::from_secs(600)).context("Compile Rust plugin in sandbox")?;
    let output = staging
        .join("target")
        .join(TARGET)
        .join("release")
        .join(format!("{output_name}.wasm"));
    let metadata = std::fs::metadata(&output)
        .context("Rust plugin did not produce the declared WASM library")?;
    if metadata.len() > 32 * 1024 * 1024 {
        bail!("Compiled plugin exceeds artifact limit");
    }
    Ok(std::fs::read(output)?)
}
fn tool(rustup_home: &Path, name: &str) -> Result<PathBuf> {
    let output = Command::new("rustup")
        .args(["which", "--toolchain", TOOLCHAIN, name])
        .env("RUSTUP_HOME", rustup_home)
        .output()?;
    if !output.status.success() {
        bail!("Install Rust {TOOLCHAIN} with the {TARGET} target on the execution host");
    }
    Ok(PathBuf::from(std::str::from_utf8(&output.stdout)?.trim()).canonicalize()?)
}
fn configure(
    command: &mut Command,
    rustc: &Path,
    rustup: &Path,
    cargo_home: &Path,
    staging: &Path,
) {
    command
        .env_clear()
        .current_dir(staging)
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", staging)
        .env("TMPDIR", staging)
        .env("RUSTUP_HOME", rustup)
        .env("CARGO_HOME", cargo_home)
        .env("RUSTC", rustc)
        .env("CARGO_TARGET_DIR", staging.join("target"));
}
fn command(
    cargo: &Path,
    rustc: &Path,
    rustup: &Path,
    cargo_home: &Path,
    staging: &Path,
) -> Command {
    let mut result = Command::new(cargo);
    configure(&mut result, rustc, rustup, cargo_home, staging);
    result
}
#[cfg(target_os = "macos")]
fn sandbox(cargo: &Path, source: &Path, staging: &Path, rustup: &Path) -> Result<Command> {
    // Child processes inherit this profile, including build.rs and procedural macros.
    let quote = |path: &Path| serde_json::to_string(&path.to_string_lossy()).expect("path JSON");
    if !Path::new("/usr/bin/sandbox-exec").is_file() {
        bail!("Install the host build sandbox");
    }
    let profile = format!(
        "(version 1)(deny default)(allow process*)(allow file-read-metadata)(allow sysctl-read)(allow mach-lookup)(allow file-read* (literal \"/\") (literal \"/private/etc/ssl/openssl.cnf\") (subpath \"/System\") (subpath \"/usr\") (subpath \"/Library\") (subpath \"/bin\") (subpath \"/dev\") (subpath {}) (subpath {}) (subpath {}))(allow file-write* (subpath {}) (literal \"/dev/null\"))",
        quote(source),
        quote(staging),
        quote(rustup),
        quote(staging)
    );
    let mut result = Command::new("/usr/bin/sandbox-exec");
    result.args(["-p", &profile]).arg(cargo);
    Ok(result)
}
#[cfg(target_os = "linux")]
fn sandbox(cargo: &Path, source: &Path, staging: &Path, rustup: &Path) -> Result<Command> {
    if !Path::new("/usr/bin/bwrap").is_file() {
        bail!("Install bubblewrap for isolated plugin builds");
    }
    let mut result = Command::new("/usr/bin/bwrap");
    result.args([
        "--die-with-parent",
        "--unshare-all",
        "--new-session",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
    ]);
    for path in ["/usr", "/bin", "/lib", "/lib64"] {
        if Path::new(path).exists() {
            result.args(["--ro-bind", path, path]);
        }
    }
    result
        .arg("--ro-bind")
        .arg(source)
        .arg(source)
        .arg("--ro-bind")
        .arg(rustup)
        .arg(rustup)
        .arg("--bind")
        .arg(staging)
        .arg(staging)
        .arg("--chdir")
        .arg(staging)
        .arg(cargo);
    Ok(result)
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn sandbox(_cargo: &Path, _source: &Path, _staging: &Path, _rustup: &Path) -> Result<Command> {
    bail!("Isolated Rust plugin builds are not available on this host platform")
}
fn run(mut command: Command, timeout: Duration) -> Result<()> {
    command.stdout(Stdio::null()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .context("Unable to start isolated plugin build; check host prerequisites")?;
    let stderr = child.stderr.take().expect("piped build log");
    let reader = std::thread::spawn(move || {
        let mut retained = Vec::new();
        let mut buffer = [0u8; 8192];
        let mut stream = stderr;
        while let Ok(count) = stream.read(&mut buffer) {
            if count == 0 {
                break;
            }
            let remaining = (128 * 1024usize).saturating_sub(retained.len());
            retained.extend_from_slice(&buffer[..count.min(remaining)]);
        }
        retained
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > timeout {
            #[cfg(unix)]
            {
                if let Ok(pid) = i32::try_from(child.id()) {
                    // SAFETY: the child is placed in its own process group above.
                    unsafe {
                        libc::kill(-pid, libc::SIGKILL);
                    }
                }
            }
            let _ = child.kill();
            let _ = child.wait();
            bail!("Plugin build exceeded its time limit");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let log = reader.join().unwrap_or_default();
    if !status.success() {
        bail!(
            "Plugin preparation failed ({status}): {}",
            String::from_utf8_lossy(&log)
        );
    }
    Ok(())
}

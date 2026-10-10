//! Native Git/object and atomic filesystem primitives for shared plugin policy.
mod http;
pub mod invocations;
pub mod transport;
use openwebide_core::plugins::{
    MAX_PACKAGE_BYTES, MAX_PACKAGE_FILE_BYTES, MAX_PACKAGE_FILES, PackageFile, PackageFileKind,
    PluginError, PluginFuture, PluginHost, PluginSource, PreparedPlugin, prepare_plugin,
};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command, sync::Mutex};

/// Only host binaries contain the bundled plugin files.
fn bundled_files(source: &PluginSource) -> Option<Vec<PackageFile>> {
    #[derive(serde::Deserialize)]
    struct Bundle {
        repository: String,
        commit: String,
        plugins: Vec<Package>,
    }
    #[derive(serde::Deserialize)]
    struct Package {
        path: String,
        files: Vec<File>,
    }
    #[derive(serde::Deserialize)]
    struct File {
        path: String,
        content: String,
        executable: bool,
    }
    let bundle: Bundle = serde_json::from_str(include_str!("../bundled/plugins.json"))
        .expect("valid bundled host plugins");
    if source.repository != bundle.repository || source.commit != bundle.commit {
        return None;
    }
    bundle
        .plugins
        .into_iter()
        .find(|package| package.path == source.path)
        .map(|package| {
            package
                .files
                .into_iter()
                .map(|file| PackageFile {
                    path: file.path,
                    content: file.content.into_bytes(),
                    kind: if file.executable {
                        PackageFileKind::Executable
                    } else {
                        PackageFileKind::File
                    },
                })
                .collect()
        })
}

#[derive(Clone, Debug)]
pub struct NativePluginInstaller {
    root: Option<PathBuf>,
    lock: Arc<Mutex<()>>,
}
impl Default for NativePluginInstaller {
    fn default() -> Self {
        Self::new(default_root())
    }
}
impl NativePluginInstaller {
    pub fn new(root: Option<PathBuf>) -> Self {
        Self {
            root,
            lock: Arc::new(Mutex::new(())),
        }
    }

    pub async fn component(
        &self,
        owner: &str,
        prepared: &PreparedPlugin,
    ) -> Result<Vec<u8>, PluginError> {
        prepared.validate()?;
        if prepared.manifest.executable.is_none() {
            return Err(PluginError::Invalid(
                "Plugin does not declare an executable.".into(),
            ));
        }
        let _guard = self.lock.lock().await;
        let root = self
            .root
            .as_ref()
            .ok_or_else(|| host_error("Plugin cache is not configured."))?;
        if !root.is_absolute() {
            return Err(host_error("Plugin cache path must be absolute."));
        }
        let host = ScopedHost {
            root: root.join(key(owner)),
            host_id: prepared.host_id.clone(),
        };
        let manifest_path = host
            .snapshot(&prepared.source, &prepared.digest)
            .join("plugin.json");
        reject_link(&manifest_path)?;
        if std::fs::metadata(&manifest_path).map_err(io_error)?.len()
            > MAX_PACKAGE_FILE_BYTES as u64
        {
            return Err(host_error("Cached plugin manifest exceeds its limit."));
        }
        let manifest: openwebide_core::plugins::PluginManifest =
            serde_json::from_slice(&std::fs::read(manifest_path).map_err(io_error)?)
                .map_err(|error| host_error(error.to_string()))?;
        if manifest != prepared.manifest {
            return Err(host_error(
                "Plugin receipt does not match its prepared executable.",
            ));
        }
        load_artifact(&host.artifact(&prepared.source, &prepared.digest))
    }
    pub async fn package(
        &self,
        owner: &str,
        host_id: String,
        expected: &PreparedPlugin,
    ) -> Result<openwebide_core::plugins::PluginPackage, PluginError> {
        let _guard = self.lock.lock().await;
        let root = self
            .root
            .as_ref()
            .ok_or_else(|| host_error("Configure OPENWEBIDE_PLUGIN_DIR on this execution host."))?;
        if !root.is_absolute() {
            return Err(host_error("Plugin cache path must be absolute."));
        }
        let host = ScopedHost {
            root: root.join(key(owner)),
            host_id,
        };
        openwebide_core::plugins::load_plugin_package(&host, expected).await
    }
    pub async fn catalog(
        &self,
        owner: &str,
        source: &openwebide_core::plugins::marketplace::MarketplaceSource,
        now: i64,
    ) -> Result<openwebide_core::plugins::marketplace::CachedMarketplace, PluginError> {
        use openwebide_core::plugins::marketplace::{
            CachedMarketplace, MAX_CATALOG_BYTES, MarketplaceCatalog,
        };
        source.validate()?;
        let _guard = self.lock.lock().await;
        let root = self
            .root
            .as_ref()
            .ok_or_else(|| host_error("Configure OPENWEBIDE_PLUGIN_DIR on this execution host."))?;
        if !root.is_absolute() {
            return Err(host_error("Plugin cache path must be absolute."));
        }
        let host = ScopedHost {
            root: root.join(key(owner)),
            host_id: String::new(),
        };
        let repo = host.root.join("repositories").join(key(&source.repository));
        let parent = repo.parent().expect("repository parent");
        create_directory(parent)?;
        if !repo.exists() {
            let stage = tempfile::tempdir_in(parent).map_err(io_error)?;
            let advertised = git(
                stage.path(),
                &["ls-remote", "--", &source.repository, "HEAD"],
                4096,
                false,
            )
            .await?
            .expect("Git output");
            let hash = std::str::from_utf8(&advertised)
                .map_err(|_| host_error("Invalid Git advertisement."))?
                .split_whitespace()
                .next()
                .unwrap_or_default();
            let format = if hash.len() == 64 {
                "--object-format=sha256"
            } else {
                "--object-format=sha1"
            };
            git(stage.path(), &["init", "--bare", format, "."], 4096, false).await?;
            std::fs::rename(stage.path(), &repo).map_err(io_error)?;
        }
        reject_link(&repo)?;
        let reference = if source.reference.is_empty() {
            "HEAD"
        } else {
            &source.reference
        };
        git(
            &repo,
            &[
                "fetch",
                "--quiet",
                "--no-tags",
                "--depth=1",
                "--",
                &source.repository,
                reference,
            ],
            4096,
            false,
        )
        .await?;
        let commit = git(
            &repo,
            &["rev-parse", "--verify", "FETCH_HEAD^{commit}"],
            256,
            false,
        )
        .await?
        .expect("Git output");
        let commit = String::from_utf8(commit)
            .map_err(|_| host_error("Invalid catalog commit."))?
            .trim()
            .to_owned();
        let object = format!("{commit}:{}", source.path);
        let metadata = git(
            &repo,
            &["ls-tree", &commit, "--", &source.path],
            4096,
            false,
        )
        .await?
        .expect("Git output");
        if !metadata.starts_with(b"100644 blob ") && !metadata.starts_with(b"100755 blob ") {
            return Err(PluginError::Invalid(
                "Marketplace must be a regular Git file.".into(),
            ));
        }
        let bytes = git(
            &repo,
            &["cat-file", "blob", &object],
            MAX_CATALOG_BYTES,
            false,
        )
        .await?
        .expect("Git output");
        let snapshot = CachedMarketplace {
            source: source.clone(),
            commit,
            catalog: MarketplaceCatalog::parse(&bytes)?,
            fetched_at: now,
        };
        snapshot.validate()?;
        Ok(snapshot)
    }
    pub async fn prepare(
        &self,
        owner: &str,
        host_id: String,
        source: &PluginSource,
    ) -> Result<PreparedPlugin, PluginError> {
        source.validate()?;
        let _guard = self.lock.lock().await;
        let root = self
            .root
            .as_ref()
            .ok_or_else(|| host_error("Configure OPENWEBIDE_PLUGIN_DIR on this execution host."))?;
        if !root.is_absolute() {
            return Err(host_error(
                "OPENWEBIDE_PLUGIN_DIR must be an absolute host cache path.",
            ));
        }
        let host = ScopedHost {
            root: root.join(key(owner)),
            host_id,
        };
        prepare_plugin(&host, source).await
    }
}

pub fn default_root() -> Option<PathBuf> {
    if let Some(root) = std::env::var_os("OPENWEBIDE_PLUGIN_DIR") {
        return Some(root.into());
    }
    if let Some(root) = std::env::var_os("XDG_DATA_HOME") {
        return Some(PathBuf::from(root).join("openwebide/plugins"));
    }
    #[allow(deprecated)] // Match the native bridge's existing home-directory resolution.
    std::env::home_dir().map(|home| home.join(".local/share/openwebide/plugins"))
}
fn key(value: &str) -> String {
    openwebide_core::plugins::content_digest(value.as_bytes())
}
fn host_error(message: impl Into<String>) -> PluginError {
    PluginError::Host(message.into())
}
fn io_error(error: std::io::Error) -> PluginError {
    host_error(error.to_string())
}

struct ScopedHost {
    root: PathBuf,
    host_id: String,
}
impl ScopedHost {
    fn repository(&self, source: &PluginSource) -> PathBuf {
        self.root.join("repositories").join(key(&source.repository))
    }
    fn artifact(&self, source: &PluginSource, digest: &str) -> PathBuf {
        self.root.join("artifacts").join(key(&format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            source.repository,
            source.path,
            source.commit,
            digest,
            openwebide_plugin_runtime::build::TOOLCHAIN,
            openwebide_plugin_runtime::build::sdk_digest(),
        )))
    }
    fn snapshot(&self, source: &PluginSource, digest: &str) -> PathBuf {
        self.root
            .join("versions")
            .join(key(&format!("{}\n{}", source.repository, source.path)))
            .join(&source.commit)
            .join(digest)
    }
}

impl PluginHost for ScopedHost {
    fn host_id(&self) -> String {
        self.host_id.clone()
    }
    fn files<'a>(&'a self, source: &'a PluginSource) -> PluginFuture<'a, Vec<PackageFile>> {
        Box::pin(async move {
            if let Some(files) = bundled_files(source) {
                return Ok(files);
            }
            let repo = self.repository(source);
            create_directory(repo.parent().expect("repository parent"))?;
            if !repo.exists() {
                let stage = tempfile::tempdir_in(repo.parent().expect("repository parent"))
                    .map_err(io_error)?;
                let format = if source.commit.len() == 64 {
                    "--object-format=sha256"
                } else {
                    "--object-format=sha1"
                };
                git(stage.path(), &["init", "--bare", format, "."], 4096, false).await?;
                std::fs::rename(stage.path(), &repo).map_err(io_error)?;
            }
            reject_link(&repo)?;
            let object = format!("{}^{{commit}}", source.commit);
            if git(&repo, &["cat-file", "-e", &object], 4096, true)
                .await?
                .is_none()
            {
                git(
                    &repo,
                    &[
                        "fetch",
                        "--quiet",
                        "--no-tags",
                        "--depth=1",
                        "--",
                        &source.repository,
                        &source.commit,
                    ],
                    4096,
                    false,
                )
                .await?;
            }
            let resolved = git(&repo, &["rev-parse", "--verify", &object], 256, false)
                .await?
                .expect("successful Git output");
            if resolved.strip_suffix(b"\n").unwrap_or(&resolved) != source.commit.as_bytes() {
                return Err(PluginError::Invalid(
                    "The selected object is not the requested commit.".into(),
                ));
            }
            let tree = if source.path == "." {
                source.commit.clone()
            } else {
                format!("{}:{}", source.commit, source.path)
            };
            let entries = git(
                &repo,
                &["ls-tree", "-r", "-z", &tree],
                2 * 1024 * 1024,
                false,
            )
            .await?
            .expect("successful Git output");
            let mut files = Vec::new();
            let mut bytes = 0;
            for entry in entries
                .split(|byte| *byte == 0)
                .filter(|entry| !entry.is_empty())
            {
                if files.len() >= MAX_PACKAGE_FILES {
                    return Err(PluginError::Invalid("Too many plugin files.".into()));
                }
                let entry = std::str::from_utf8(entry)
                    .map_err(|_| PluginError::Invalid("Plugin filenames must be UTF-8.".into()))?;
                let (metadata, path) = entry
                    .split_once('\t')
                    .ok_or_else(|| host_error("Invalid Git tree response."))?;
                let fields = metadata.split_whitespace().collect::<Vec<_>>();
                if fields.len() != 3 {
                    return Err(host_error("Invalid Git tree metadata."));
                }
                let kind = match fields[0] {
                    "100644" => PackageFileKind::File,
                    "100755" => PackageFileKind::Executable,
                    "120000" => PackageFileKind::Symlink,
                    "160000" => PackageFileKind::Submodule,
                    _ => return Err(PluginError::Invalid("Unsupported Git file mode.".into())),
                };
                let content = if matches!(kind, PackageFileKind::File | PackageFileKind::Executable)
                {
                    git(
                        &repo,
                        &["cat-file", "blob", fields[2]],
                        MAX_PACKAGE_FILE_BYTES,
                        false,
                    )
                    .await?
                    .expect("successful Git output")
                } else {
                    Vec::new()
                };
                bytes += content.len();
                if bytes > MAX_PACKAGE_BYTES {
                    return Err(PluginError::Invalid("Plugin package is too large.".into()));
                }
                files.push(PackageFile {
                    path: path.into(),
                    content,
                    kind,
                });
            }
            Ok(files)
        })
    }
    fn compile<'a>(
        &'a self,
        source: &'a PluginSource,
        digest: &'a str,
        manifest: &'a openwebide_core::plugins::PluginManifest,
    ) -> PluginFuture<'a, ()> {
        Box::pin(async move {
            let artifact = self.artifact(source, digest);
            let snapshot = self.snapshot(source, digest);
            let manifest = manifest.clone();
            tokio::task::spawn_blocking(move || {
                if artifact.exists() {
                    load_artifact(&artifact)?;
                    return Ok(());
                }
                let parent = artifact.parent().expect("artifact parent");
                create_directory(parent)?;
                let stage = tempfile::tempdir_in(parent).map_err(io_error)?;
                let rust = manifest.executable.as_ref().expect("Rust manifest");
                let build_dir = stage.path().join("build");
                std::fs::create_dir(&build_dir).map_err(io_error)?;
                let bytes =
                    openwebide_plugin_runtime::build::compile(&snapshot, &rust.library, &build_dir)
                        .map_err(|error| host_error(format!("{error:#}")))?;
                let runtime = openwebide_plugin_runtime::Runtime::new()
                    .map_err(|error| host_error(error.to_string()))?;
                let tools = runtime
                    .tools(&bytes, NoPluginServices, &[])
                    .map_err(|error| host_error(format!("Invalid plugin interface: {error:#}")))?;
                let tools: Vec<openwebide_core::plugins::PluginTool> = serde_json::from_value(
                    serde_json::to_value(tools).map_err(|error| host_error(error.to_string()))?,
                )
                .map_err(|error| host_error(error.to_string()))?;
                if tools != manifest.contributions.tools {
                    return Err(PluginError::Invalid(
                        "Compiled tools do not match the manifest.".into(),
                    ));
                }
                let events = runtime
                    .events(&bytes)
                    .map_err(|error| host_error(format!("Invalid plugin events: {error:#}")))?;
                if events != manifest.contributions.events {
                    return Err(PluginError::Invalid(
                        "Compiled events do not match the manifest.".into(),
                    ));
                }
                // Build artifacts are disposable; retain only the validated component.
                std::fs::remove_dir_all(&build_dir).map_err(io_error)?;
                std::fs::write(stage.path().join("plugin.wasm"), &bytes).map_err(io_error)?;
                std::fs::write(
                    stage.path().join("digest"),
                    openwebide_core::plugins::content_digest(&bytes),
                )
                .map_err(io_error)?;
                std::fs::rename(stage.path(), &artifact).map_err(io_error)?;
                Ok(())
            })
            .await
            .map_err(|error| host_error(error.to_string()))?
        })
    }
    fn publish<'a>(
        &'a self,
        source: &'a PluginSource,
        digest: &'a str,
        files: &'a [PackageFile],
    ) -> PluginFuture<'a, ()> {
        Box::pin(async move {
            let destination = self.snapshot(source, digest);
            let parent = destination.parent().expect("snapshot parent");
            create_directory(parent)?;
            if destination.exists() {
                return verify_snapshot(&destination, files);
            }
            let stage = tempfile::tempdir_in(parent).map_err(io_error)?;
            for file in files {
                let target = stage.path().join(&file.path);
                std::fs::create_dir_all(target.parent().expect("file parent")).map_err(io_error)?;
                std::fs::write(&target, &file.content).map_err(io_error)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = if file.kind == PackageFileKind::Executable {
                        0o500
                    } else {
                        0o400
                    };
                    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))
                        .map_err(io_error)?;
                }
            }
            std::fs::rename(stage.path(), &destination).map_err(io_error)?;
            verify_snapshot(&destination, files)
        })
    }
}

struct NoPluginServices;
impl openwebide_plugin_runtime::HostServices for NoPluginServices {
    fn request(&mut self, _capability: &str, _payload: &str) -> Result<String, String> {
        Err("Host capabilities are unavailable during interface validation.".into())
    }
}
fn load_artifact(path: &Path) -> Result<Vec<u8>, PluginError> {
    reject_link(path)?;
    let file = path.join("plugin.wasm");
    reject_link(&file)?;
    let digest = path.join("digest");
    reject_link(&digest)?;
    if std::fs::metadata(&file).map_err(io_error)?.len() > 32 * 1024 * 1024 {
        return Err(host_error("Cached plugin artifact exceeds its limit."));
    }
    let bytes = std::fs::read(file).map_err(io_error)?;
    if std::fs::read_to_string(digest).map_err(io_error)?
        != openwebide_core::plugins::content_digest(&bytes)
    {
        return Err(host_error("Cached plugin artifact has changed."));
    }
    Ok(bytes)
}

fn reject_link(path: &Path) -> Result<(), PluginError> {
    if std::fs::symlink_metadata(path)
        .map_err(io_error)?
        .file_type()
        .is_symlink()
    {
        return Err(host_error("Plugin cache paths cannot be symbolic links."));
    }
    Ok(())
}
fn create_directory(path: &Path) -> Result<(), PluginError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        if !parent.exists() {
            create_directory(parent)?;
        } else {
            reject_link(parent)?;
        }
    }
    std::fs::create_dir_all(path).map_err(io_error)?;
    reject_link(path)
}
fn verify_snapshot(root: &Path, files: &[PackageFile]) -> Result<(), PluginError> {
    reject_link(root)?;
    let mut pending = vec![root.to_path_buf()];
    let mut count = 0;
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            let kind = entry.file_type().map_err(io_error)?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                count += 1;
            } else {
                return Err(host_error(
                    "The cached plugin snapshot contains an unsupported file.",
                ));
            }
            if count > files.len() {
                return Err(host_error(
                    "The cached plugin snapshot contains extra files.",
                ));
            }
        }
    }
    if count != files.len() {
        return Err(host_error("The cached plugin snapshot has missing files."));
    }
    for file in files {
        let mut target = root.to_path_buf();
        for part in file.path.split('/') {
            target.push(part);
            reject_link(&target)?;
        }
        let metadata = std::fs::metadata(&target).map_err(io_error)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let executable = metadata.permissions().mode() & 0o111 != 0;
            if executable != (file.kind == PackageFileKind::Executable) {
                return Err(host_error("The cached plugin snapshot file mode changed."));
            }
        }
        if !metadata.is_file()
            || metadata.len() != file.content.len() as u64
            || std::fs::read(&target).map_err(io_error)? != file.content
        {
            return Err(host_error(
                "The cached plugin snapshot changed; prepare a clean host cache.",
            ));
        }
    }
    Ok(())
}

async fn git(
    cwd: &Path,
    args: &[&str],
    limit: usize,
    allow_failure: bool,
) -> Result<Option<Vec<u8>>, PluginError> {
    let mut command = Command::new("git");
    command
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "protocol.ext.allow=never",
            "-c",
            "protocol.file.allow=never",
        ])
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().map_err(io_error)?;
    #[cfg(unix)]
    let group_guard = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .map(crate::exec::proc::GroupGuard::new);
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let result = tokio::time::timeout(Duration::from_secs(90), async {
        let out = async {
            let mut bytes = Vec::new();
            stdout
                .take(limit as u64 + 1)
                .read_to_end(&mut bytes)
                .await?;
            Ok::<_, std::io::Error>(bytes)
        };
        let err = async {
            let mut bytes = Vec::new();
            stderr.take(8192).read_to_end(&mut bytes).await?;
            Ok::<_, std::io::Error>(bytes)
        };
        tokio::try_join!(out, err, child.wait())
    })
    .await
    .map_err(|_| host_error("Plugin Git request timed out."))?
    .map_err(io_error)?;
    #[cfg(unix)]
    if let Some(guard) = group_guard {
        guard.disarm();
    }
    if result.0.len() > limit {
        return Err(PluginError::Invalid(
            "Plugin Git object exceeds its content limit.".into(),
        ));
    }
    if !result.2.success() {
        if allow_failure {
            return Ok(None);
        }
        tracing::warn!(detail = %String::from_utf8_lossy(&result.1), "Plugin Git request failed");
        return Err(host_error(
            "Could not fetch/read the pinned plugin commit. Check the repository and host Git credentials.",
        ));
    }
    Ok(Some(result.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use openwebide_core::plugins::{
        marketplace::MarketplaceSource,
        testing::{review_files, source},
    };

    fn command(cwd: &Path, args: &[&str]) -> String {
        let result = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8(result.stdout).unwrap().trim().into()
    }
    fn seed(root: &Path, owner: &str) -> PluginSource {
        let fixture = tempfile::tempdir().unwrap();
        for file in review_files() {
            let path = fixture.path().join("plugins/pr-review").join(file.path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, file.content).unwrap();
        }
        command(fixture.path(), &["init", "-q"]);
        command(fixture.path(), &["add", "."]);
        command(
            fixture.path(),
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.org",
                "commit",
                "-qm",
                "fixture",
            ],
        );
        let mut input = source();
        input.commit = command(fixture.path(), &["rev-parse", "HEAD"]);
        let mut catalog = openwebide_core::plugins::testing::catalog().catalog;
        catalog.plugins[0].releases[0].source.commit = input.commit.clone();
        std::fs::write(
            fixture.path().join("marketplace.json"),
            serde_json::to_vec(&catalog).unwrap(),
        )
        .unwrap();
        command(fixture.path(), &["add", "marketplace.json"]);
        command(
            fixture.path(),
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.org",
                "commit",
                "-qm",
                "catalog",
            ],
        );

        let scoped = ScopedHost {
            root: root.join(key(owner)),
            host_id: "host".into(),
        };
        let repository = scoped.repository(&input);
        std::fs::create_dir_all(repository.parent().unwrap()).unwrap();
        command(
            root,
            &[
                "clone",
                "--bare",
                fixture.path().to_str().unwrap(),
                repository.to_str().unwrap(),
            ],
        );
        input
    }

    #[tokio::test]
    async fn native_hosts_materialize_pinned_git_objects_atomically_and_reuse_offline() {
        for (owner, id) in [("paired", "local-host"), ("user:1", "remote-host")] {
            let root = tempfile::tempdir().unwrap();
            let source = seed(root.path(), owner);
            let installer = NativePluginInstaller::new(Some(root.path().into()));
            let prepared = installer.prepare(owner, id.into(), &source).await.unwrap();
            assert_eq!(prepared.manifest.name, "pr-review");
            assert_eq!(prepared.host_id, id);
            let snapshot = ScopedHost {
                root: root.path().join(key(owner)),
                host_id: id.into(),
            }
            .snapshot(&source, &prepared.digest);
            assert_eq!(
                std::fs::read(snapshot.join("plugin.json")).unwrap(),
                review_files()[0].content
            );
            let (first, second) = tokio::join!(
                installer.prepare(owner, id.into(), &source),
                installer.prepare(owner, id.into(), &source)
            );
            assert_eq!(first.unwrap(), prepared);
            assert_eq!(second.unwrap(), prepared);
            let mut wrong_path = source.clone();
            wrong_path.path = "missing".into();
            assert!(
                installer
                    .prepare(owner, id.into(), &wrong_path)
                    .await
                    .is_err()
            );
            assert!(snapshot.join("plugin.json").exists());
        }
    }
    #[tokio::test]
    async fn uncached_commit_is_fetched_from_a_public_git_transport() {
        let root = tempfile::tempdir().unwrap();
        let original = seed(root.path(), "seed");
        let host = ScopedHost {
            root: root.path().join(key("seed")),
            host_id: "seed".into(),
        };
        let repository = host.repository(&original);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let mut daemon = Command::new("git")
            .args([
                "daemon",
                "--export-all",
                "--reuseaddr",
                "--listen=127.0.0.1",
                &format!("--port={port}"),
                &format!("--base-path={}", repository.parent().unwrap().display()),
                repository.parent().unwrap().to_str().unwrap(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut ready = false;
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ready);
        let mut source = original;
        source.repository = format!(
            "git://127.0.0.1:{port}/{}",
            repository.file_name().unwrap().to_str().unwrap()
        );
        let installer = NativePluginInstaller::new(Some(root.path().into()));
        let marketplace = MarketplaceSource {
            repository: source.repository.clone(),
            reference: String::new(),
            path: "marketplace.json".into(),
        };
        let catalog = installer.catalog("user:1", &marketplace, 1).await.unwrap();
        let plugin = &catalog.catalog.plugins[0];
        assert_eq!(
            catalog
                .catalog
                .resolve(
                    &marketplace,
                    &plugin.publisher,
                    &plugin.name,
                    &plugin.releases[0].version
                )
                .unwrap(),
            source
        );
        let prepared = installer
            .prepare("user:1", "remote-host".into(), &source)
            .await
            .unwrap();
        daemon.kill().await.unwrap();
        assert_eq!(prepared.manifest.name, "pr-review");
        let package = installer
            .package("user:1", "remote-host".into(), &prepared)
            .await
            .unwrap();
        assert_eq!(package.prepared, prepared);
        assert!(
            package.skills[0]
                .resources
                .iter()
                .any(|r| r.name == "references/review-checks.md")
        );
        assert_eq!(
            installer
                .prepare("user:1", "remote-host".into(), &source)
                .await
                .unwrap(),
            prepared
        );
    }
    #[tokio::test]
    async fn corrupted_cache_is_not_reported_as_ready_or_repaired_in_place() {
        let root = tempfile::tempdir().unwrap();
        let source = seed(root.path(), "paired");
        let installer = NativePluginInstaller::new(Some(root.path().into()));
        let prepared = installer
            .prepare("paired", "host".into(), &source)
            .await
            .unwrap();
        let snapshot = ScopedHost {
            root: root.path().join(key("paired")),
            host_id: "host".into(),
        }
        .snapshot(&source, &prepared.digest);
        let file = snapshot.join("README.md");
        std::fs::remove_file(&file).unwrap();
        std::fs::write(&file, "corrupted").unwrap();
        assert!(
            installer
                .prepare("paired", "host".into(), &source)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(file).unwrap(), "corrupted");
    }
    #[tokio::test]
    #[ignore = "Smoke test against the published official marketplace; requires network access"]
    async fn official_marketplace_reference_package_installs_and_loads_on_both_host_scopes() {
        let root = tempfile::tempdir().unwrap();
        let installer = NativePluginInstaller::new(Some(root.path().into()));
        for (owner, host) in [("paired", "local-host"), ("user:1", "remote-host")] {
            let source = MarketplaceSource::official();
            let catalog = installer.catalog(owner, &source, 1).await.unwrap();
            let plugin = catalog
                .catalog
                .plugins
                .iter()
                .find(|p| p.publisher == "openwebide" && p.name == "pr-review")
                .unwrap();
            let release = &plugin.releases[0];
            let source = catalog
                .catalog
                .resolve(&source, &plugin.publisher, &plugin.name, &release.version)
                .unwrap();
            let prepared = installer
                .prepare(owner, host.into(), &source)
                .await
                .unwrap();
            assert_eq!(prepared.manifest.version, release.version);
            let package = installer
                .package(owner, host.into(), &prepared)
                .await
                .unwrap();
            package.validate().unwrap();
            assert!(!package.skills.is_empty());
            assert!(!package.skills[0].resources.is_empty());
        }
    }

    #[tokio::test]
    async fn preparation_requires_configured_host_and_rejects_unpinned_requests() {
        let installer = NativePluginInstaller::new(None);
        assert!(matches!(
            installer.prepare("paired", "host".into(), &source()).await,
            Err(PluginError::Host(_))
        ));
        let mut source = source();
        source.commit = "main".into();
        assert!(matches!(
            installer.prepare("paired", "host".into(), &source).await,
            Err(PluginError::Invalid(_))
        ));
    }
}

#[cfg(test)]
mod bundled_tests {
    use super::*;
    #[tokio::test]
    async fn bundled_core_plugins_prepare_offline_on_both_hosts_using_normal_validation() {
        let sources = openwebide_core::plugins::bundled_plugin_sources();
        assert_eq!(sources.len(), 4);
        for host in ["server-host", "paired-local-host"] {
            let root = tempfile::tempdir().unwrap();
            let installer = NativePluginInstaller::new(Some(root.path().to_path_buf()));
            for source in &sources {
                let prepared = installer
                    .prepare("owner", host.into(), source)
                    .await
                    .unwrap();
                let package = installer
                    .package("owner", host.into(), &prepared)
                    .await
                    .unwrap();
                assert_eq!(package.prepared, prepared);
                assert_eq!(prepared.host_id, host);
                assert_eq!(prepared.manifest.contributions.tool_groups.len(), 1);
                assert_eq!(prepared.manifest.version, "0.1.0");
                if prepared.manifest.name == "skill-authoring" {
                    assert_eq!(package.skills.len(), 1);
                } else {
                    assert!(package.skills.is_empty());
                }
            }
            assert!(!root.path().join(key("owner")).join("repositories").exists());
        }
        let mut optional = sources[0].clone();
        optional.path = "plugins/pr-review".into();
        assert!(bundled_files(&optional).is_none());
        let mut future = sources[0].clone();
        future.commit = "b".repeat(40);
        assert!(bundled_files(&future).is_none());
    }
}

#[cfg(test)]
mod rust_plugin_tests {
    use super::*;
    use openwebide_core::plugins::execution;
    struct DatabaseHost {
        store: openwebide_storage::Store<openwebide_storage::rusqlite_db::RusqliteDb>,
        user: openwebide_core::UserId,
        session: i64,
    }
    impl openwebide_agent::plugins::execution::GrantedHost for DatabaseHost {
        async fn request(&self, request: &execution::PluginHostRequest) -> Result<String, String> {
            self.store
                .plugin_host_request(self.user, self.session, request, 10)
                .await
                .map_err(|error| error.to_string())
        }
    }
    struct NoBuiltin;
    impl openwebide_agent::ToolExecutor for NoBuiltin {
        fn describe(&self, call: &openwebide_core::ToolCall) -> String {
            call.name.clone()
        }
        async fn execute(&self, _: &openwebide_core::ToolCall) -> openwebide_agent::ToolOutcome {
            panic!("Plugin tool must not fall through to a built-in executor")
        }
    }
    async fn exercise_records(
        installer: &NativePluginInstaller,
        owner: &str,
        prepared: &PreparedPlugin,
    ) {
        use openwebide_agent::{
            ToolExecutor,
            plugins::execution::{GrantedServices, PluginTools},
        };
        use openwebide_core::{
            NewProject, UserRole, WorkspaceMode,
            plugins::{PluginPackage, RecordPlugin},
        };
        let store = openwebide_storage::Store::new(
            openwebide_storage::rusqlite_db::RusqliteDb::open_in_memory().unwrap(),
        );
        store.migrate().await.unwrap();
        let user = store
            .insert_user("owner", "hash", UserRole::Admin, 0)
            .await
            .unwrap()
            .id;
        let mode = if owner == "paired" {
            WorkspaceMode::Local
        } else {
            WorkspaceMode::Remote
        };
        let project = store
            .create_project(
                &NewProject {
                    name: "project".into(),
                    mode,
                    path: Some("p".into()),
                },
                user,
                0,
            )
            .await
            .unwrap()
            .id;
        let session = store
            .create_session("session", None, None, Some(project), user, 0)
            .await
            .unwrap()
            .id;
        store
            .record_plugin(
                user,
                &RecordPlugin {
                    approved_capabilities: Vec::new(),
                    prepared: prepared.clone(),
                    revision: None,
                    update_policy: None,
                    package: Some(Box::new(PluginPackage {
                        prepared: prepared.clone(),
                        skills: Vec::new(),
                    })),
                },
                1,
            )
            .await
            .unwrap();
        let token = "d".repeat(32);
        store
            .issue_plugin_grant(user, session, prepared, &token, 2)
            .await
            .unwrap();
        let executor = PluginTools {
            executor: NoBuiltin,
            transport: transport::NativePluginTransport {
                installer: installer.clone(),
                invocations: invocations::Invocations::default(),
                owner: owner.into(),
            },
            services: GrantedServices {
                host: DatabaseHost {
                    store,
                    user,
                    session,
                },
                grants: Arc::new([(prepared.digest.clone(), token)].into_iter().collect()),
            },
            plugins: Arc::new(vec![prepared.clone()]),
        };
        let call = openwebide_core::ToolCall {id:"fixture".into(), name:"fixture_echo".into(),
            arguments:serde_json::json!({"collection":"notes","operation":{"action":"create","value":{"text":"plugin-owned state"}}}).to_string()};
        let outcome = executor.execute(&call).await;
        assert!(outcome.ok, "{}", outcome.content);
        let result: serde_json::Value = serde_json::from_str(&outcome.content).unwrap();
        assert_eq!(result["records"][0]["value"]["text"], "plugin-owned state");
        let id = result["records"][0]["id"].as_i64().unwrap();
        let read = openwebide_core::ToolCall {
            arguments:
                serde_json::json!({"collection":"notes","operation":{"action":"read","id":id}})
                    .to_string(),
            ..call
        };
        let outcome = executor.execute(&read).await;
        assert!(outcome.ok);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&outcome.content).unwrap(),
            result
        );
    }
    #[tokio::test]
    async fn source_compilation_validates_tools_caches_offline_and_rejects_artifact_corruption() {
        let root = tempfile::tempdir().unwrap();
        let manifest: openwebide_core::plugins::PluginManifest = serde_json::from_value(serde_json::json!({
            "schemaVersion":1,"publisher":"example","name":"fixture","version":"0.1.0",
            "displayName":"Fixture","description":"SDK adapter contract","license":"MIT",
            "compatibility":{"pluginApi":3},
            "executable":{"manifest":"Cargo.toml","library":"sdk_fixture","sdkVersion":"0.1.0","capabilities":["records"]},
            "contributions":{"skills":[],"events":["job_due"],"tools":[{"name":"fixture_echo","description":"Exercise the public host contract.","parameters":{"type":"object","properties":{}},"requires_approval":false}]}
        })).unwrap();
        let cargo = include_str!("../../crates/plugin-sdk/examples/fixture/Cargo.toml")
            .replace("{ path = \"../..\" }", "\"=0.1.0\"");
        let files = vec![
            ("plugin.json", serde_json::to_string(&manifest).unwrap()),
            ("Cargo.toml", cargo),
            (
                "Cargo.lock",
                include_str!("../../crates/plugin-sdk/examples/fixture/Cargo.lock").into(),
            ),
            (
                "src/lib.rs",
                include_str!("../../crates/plugin-sdk/examples/fixture/src/lib.rs").into(),
            ),
        ]
        .into_iter()
        .map(|(path, content)| PackageFile {
            path: path.into(),
            content: content.into_bytes(),
            kind: PackageFileKind::File,
        })
        .collect::<Vec<_>>();
        openwebide_core::plugins::validate_files(&files).unwrap();
        let source = openwebide_core::plugins::testing::source();
        let digest = openwebide_core::plugins::package_digest(&files);
        let installer = NativePluginInstaller::new(Some(root.path().to_path_buf()));
        for host_id in ["server-host", "paired-local-host"] {
            let owner = if host_id == "server-host" {
                "user:1"
            } else {
                "paired"
            };
            let host = ScopedHost {
                root: root.path().join(key(owner)),
                host_id: host_id.into(),
            };
            host.publish(&source, &digest, &files).await.unwrap();
            host.compile(&source, &digest, &manifest).await.unwrap();
            let bytes = load_artifact(&host.artifact(&source, &digest)).unwrap();
            assert!(!bytes.is_empty());
            let prepared = PreparedPlugin {
                source: source.clone(),
                manifest: manifest.clone(),
                digest: digest.clone(),
                host_id: host_id.into(),
            };
            exercise_records(&installer, owner, &prepared).await;
            let invocations = invocations::Invocations::default();
            let event = invocations
                .start(
                    &installer,
                    owner,
                    execution::InvokePlugin {
                        operation: execution::PluginOperation::Event,
                        prepared: prepared.clone(),
                        name: String::new(),
                        arguments: serde_json::json!({"name":"job_due","payload":{"job_id":7}})
                            .to_string(),
                    },
                )
                .await
                .unwrap();
            let event_import = invocations
                .resume(
                    owner,
                    execution::ContinuePlugin {
                        id: event.id.clone(),
                        sequence: 0,
                        response: Ok(String::new()),
                    },
                )
                .await
                .unwrap();
            let execution::PluginStep::HostCall {
                capability,
                payload,
                ..
            } = event_import.step
            else {
                panic!("event did not execute plugin code")
            };
            assert_eq!(capability, "records");
            let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
            assert_eq!(value["operation"]["value"]["event"], "job_due");
            assert_eq!(value["operation"]["value"]["payload"]["job_id"], 7);
            let event_result = invocations
                .resume(
                    owner,
                    execution::ContinuePlugin {
                        id: event.id,
                        sequence: 1,
                        response: Ok("{\"stored\":true}".into()),
                    },
                )
                .await
                .unwrap();
            assert!(matches!(
                event_result.step,
                execution::PluginStep::Complete { ok: true, .. }
            ));
            let context = invocations
                .start(
                    &installer,
                    owner,
                    execution::InvokePlugin {
                        operation: execution::PluginOperation::Context,
                        prepared: prepared.clone(),
                        name: String::new(),
                        arguments: "{\"budget_bytes\":64}".into(),
                    },
                )
                .await
                .unwrap();
            let context_read = invocations
                .resume(
                    owner,
                    execution::ContinuePlugin {
                        id: context.id.clone(),
                        sequence: 0,
                        response: Ok(String::new()),
                    },
                )
                .await
                .unwrap();
            let execution::PluginStep::HostCall {
                capability,
                payload,
                ..
            } = context_read.step
            else {
                panic!("context did not request its general record read");
            };
            assert_eq!(capability, "records");
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&payload).unwrap()["operation"]["action"],
                "list"
            );
            let complete = invocations
                .resume(
                    owner,
                    execution::ContinuePlugin {
                        id: context.id,
                        sequence: 1,
                        response: Ok("{}".into()),
                    },
                )
                .await
                .unwrap();
            let execution::PluginStep::Complete {
                content, ok: true, ..
            } = complete.step
            else {
                panic!("context did not complete");
            };
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&content).unwrap()["prompt"],
                "Stored fact"
            );
            let mutation = invocations
                .start(
                    &installer,
                    owner,
                    execution::InvokePlugin {
                        operation: execution::PluginOperation::Context,
                        prepared: prepared.clone(),
                        name: String::new(),
                        arguments: "{\"budget_bytes\":1}".into(),
                    },
                )
                .await
                .unwrap();
            let failed = invocations
                .resume(
                    owner,
                    execution::ContinuePlugin {
                        id: mutation.id,
                        sequence: 0,
                        response: Ok(String::new()),
                    },
                )
                .await
                .unwrap();
            assert!(matches!(failed.step, execution::PluginStep::Failed { .. }));
            let ready = invocations
                .start(
                    &installer,
                    owner,
                    execution::InvokePlugin {
                        operation: execution::PluginOperation::Tool,
                        prepared: prepared.clone(),
                        name: "fixture_echo".into(),
                        arguments: "{}".into(),
                    },
                )
                .await
                .unwrap();
            assert!(matches!(ready.step, execution::PluginStep::Ready));
            assert!(
                invocations
                    .cancel("another-owner", &ready.id)
                    .await
                    .is_err()
            );
            let callback = invocations
                .resume(
                    owner,
                    execution::ContinuePlugin {
                        id: ready.id.clone(),
                        sequence: 0,
                        response: Ok(String::new()),
                    },
                )
                .await
                .unwrap();
            assert!(matches!(
                callback.step,
                execution::PluginStep::HostCall { sequence: 1, .. }
            ));
            let response = execution::ContinuePlugin {
                id: ready.id.clone(),
                sequence: 1,
                response: Ok("{\"value\":42}".into()),
            };
            assert!(
                invocations
                    .resume("another-owner", response.clone())
                    .await
                    .is_err()
            );
            let stale = execution::ContinuePlugin {
                sequence: 0,
                ..response.clone()
            };
            assert!(invocations.resume(owner, stale).await.is_err());
            let result = invocations.resume(owner, response.clone()).await.unwrap();
            assert!(matches!(
                result.step,
                execution::PluginStep::Complete { ok: true, .. }
            ));
            assert!(invocations.resume(owner, response).await.is_err());
            let ready = invocations
                .start(
                    &installer,
                    owner,
                    execution::InvokePlugin {
                        operation: execution::PluginOperation::Tool,
                        prepared,
                        name: "fixture_echo".into(),
                        arguments: "{}".into(),
                    },
                )
                .await
                .unwrap();
            let callback = invocations
                .resume(
                    owner,
                    execution::ContinuePlugin {
                        id: ready.id.clone(),
                        sequence: 0,
                        response: Ok(String::new()),
                    },
                )
                .await
                .unwrap();
            assert!(matches!(
                callback.step,
                execution::PluginStep::HostCall { sequence: 1, .. }
            ));
            invocations.cancel(owner, &ready.id).await.unwrap();
            assert!(
                invocations
                    .resume(
                        owner,
                        execution::ContinuePlugin {
                            id: ready.id,
                            sequence: 1,
                            response: Ok("{}".into()),
                        }
                    )
                    .await
                    .is_err()
            );
        }
        let host = ScopedHost {
            root: root.path().join(key("user:1")),
            host_id: "server-host".into(),
        };
        std::fs::write(
            host.artifact(&source, &digest).join("plugin.wasm"),
            b"corrupt",
        )
        .unwrap();
        assert!(host.compile(&source, &digest, &manifest).await.is_err());
    }
}

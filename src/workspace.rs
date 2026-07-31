use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use cap_std::ambient_authority;
use cap_std::fs::{Dir, File, OpenOptions};

#[derive(Clone, Debug)]
pub struct Workspace {
    root: PathBuf,
    dir: Arc<Dir>,
    /// The startup policy file; rerooting never changes this anchor.
    policy_anchor: PathBuf,
    external: Arc<Vec<ExternalRoot>>,
}

#[derive(Debug)]
struct ExternalRoot {
    path: PathBuf,
    writable: bool,
    capability: ExternalCapability,
}

#[derive(Debug)]
enum ExternalCapability {
    Dir(Arc<Dir>),
    File(Arc<Mutex<File>>),
}

impl Workspace {
    pub fn new(root: impl AsRef<Path>) -> Result<Self, String> {
        let root = std::fs::canonicalize(root.as_ref())
            .map_err(|error| format!("cannot canonicalize workspace: {error}"))?;
        let dir = Dir::open_ambient_dir(&root, ambient_authority())
            .map_err(|error| format!("cannot open workspace: {error}"))?;
        Ok(Self {
            policy_anchor: root.join(".e-agent/config.toml"),
            root,
            dir: Arc::new(dir),
            external: Arc::new(Vec::new()),
        })
    }

    /// Install a resolved policy. Ambient authority is used only once here to
    /// turn canonical configured roots into durable capabilities.
    pub fn with_external_roots(mut self, sandbox: &crate::config::Sandbox) -> Result<Self, String> {
        let mut roots = Vec::new();
        for (writable, paths) in [
            (true, &sandbox.writable_paths),
            (false, &sandbox.readable_paths),
        ] {
            for path in paths {
                let path = PathBuf::from(path);
                let metadata = std::fs::metadata(&path).map_err(|error| {
                    format!("cannot inspect external root {}: {error}", path.display())
                })?;
                let capability = if metadata.is_dir() {
                    ExternalCapability::Dir(Arc::new(
                        Dir::open_ambient_dir(&path, ambient_authority()).map_err(|error| {
                            format!("cannot open external directory {}: {error}", path.display())
                        })?,
                    ))
                } else {
                    let mut options = OpenOptions::new();
                    options.read(true).write(writable);
                    ExternalCapability::File(Arc::new(Mutex::new(
                        File::open_ambient_with(&path, &options, ambient_authority()).map_err(
                            |error| {
                                format!("cannot open external file {}: {error}", path.display())
                            },
                        )?,
                    )))
                };
                roots.push(ExternalRoot {
                    path,
                    writable,
                    capability,
                });
            }
        }
        self.external = Arc::new(roots);
        Ok(self)
    }

    /// Derive a workspace only from authority already held by this workspace.
    pub fn reroot(&self, root: impl AsRef<Path>) -> Result<Self, String> {
        let requested = root.as_ref();
        if requested == Path::new("/")
            || !requested.is_absolute()
            || requested.components().any(|part| {
                !matches!(
                    part,
                    Component::RootDir | Component::Prefix(_) | Component::Normal(_)
                )
            })
        {
            return Err(
                "custom workspace must be an absolute canonical authorized directory".into(),
            );
        }
        if let Ok(remainder) = requested.strip_prefix(&self.root) {
            return self.reroot_from(&self.root, &self.dir, remainder);
        }
        for external in self.external.iter().filter(|root| root.writable) {
            if let ExternalCapability::Dir(dir) = &external.capability
                && let Ok(remainder) = requested.strip_prefix(&external.path)
            {
                return self.reroot_from(&external.path, dir, remainder);
            }
        }
        Err("custom workspace is not within the workspace or an authorized writable external directory".into())
    }

    fn reroot_from(&self, base: &Path, dir: &Arc<Dir>, remainder: &Path) -> Result<Self, String> {
        if !remainder.as_os_str().is_empty() {
            let canonical = dir
                .canonicalize(remainder)
                .map_err(|error| format!("cannot open authorized custom workspace: {error}"))?;
            if canonical != remainder {
                return Err(
                    "custom workspace must be canonical and contain no symlink aliases".into(),
                );
            }
        }
        let dir = if remainder.as_os_str().is_empty() {
            dir.try_clone()
        } else {
            dir.open_dir(remainder)
        }
        .map_err(|error| format!("cannot open authorized custom workspace: {error}"))?;
        Ok(Self {
            root: base.join(remainder),
            dir: Arc::new(dir),
            policy_anchor: self.policy_anchor.clone(),
            external: self.external.clone(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn policy_anchor(&self) -> &Path {
        &self.policy_anchor
    }

    pub fn policy_anchor_is_visible(&self) -> bool {
        self.policy_anchor.starts_with(&self.root)
            || self
                .external
                .iter()
                .any(|root| self.policy_anchor.starts_with(&root.path))
    }

    pub fn read(&self, input: &str) -> Result<Vec<u8>, String> {
        let path = Path::new(input);
        if !path.is_absolute() {
            return self
                .dir
                .read(self.relative(input)?)
                .map_err(|error| format!("read failed: {error}"));
        }
        let (root, remainder) = self.external(path, false)?;
        match &root.capability {
            ExternalCapability::Dir(dir) => dir
                .read(remainder)
                .map_err(|error| format!("read failed: {error}")),
            ExternalCapability::File(file) if remainder.as_os_str().is_empty() => {
                let mut file = file.lock().map_err(|_| {
                    "read failed: external file capability lock is poisoned".to_owned()
                })?;
                file.seek(SeekFrom::Start(0))
                    .map_err(|error| format!("read failed: {error}"))?;
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)
                    .map_err(|error| format!("read failed: {error}"))?;
                Ok(bytes)
            }
            ExternalCapability::File(_) => {
                Err("external file capability does not authorize sibling paths".into())
            }
        }
    }

    pub fn read_to_string(&self, input: &str) -> Result<String, String> {
        String::from_utf8(self.read(input)?).map_err(|error| format!("read failed: {error}"))
    }

    pub fn write(&self, input: &str, content: impl AsRef<[u8]>) -> Result<(), String> {
        let path = Path::new(input);
        if !path.is_absolute() {
            let path = self.relative(input)?;
            self.reject_policy_write(path)?;
            return secure_dir_write(&self.dir, path, content.as_ref());
        }
        self.reject_policy_write(path)?;
        let (root, remainder) = self.external(path, true)?;
        match &root.capability {
            ExternalCapability::Dir(dir) => secure_dir_write(dir, remainder, content.as_ref()),
            ExternalCapability::File(file) if remainder.as_os_str().is_empty() => {
                let mut file = file.lock().map_err(|_| {
                    "write failed: external file capability lock is poisoned".to_owned()
                })?;
                file.set_len(0)
                    .map_err(|error| format!("write failed: {error}"))?;
                file.seek(SeekFrom::Start(0))
                    .map_err(|error| format!("write failed: {error}"))?;
                file.write_all(content.as_ref())
                    .and_then(|()| file.flush())
                    .map_err(|error| format!("write failed: {error}"))
            }
            ExternalCapability::File(_) => {
                Err("external file capability does not authorize sibling paths".into())
            }
        }
    }

    fn reject_policy_write(&self, path: &Path) -> Result<(), String> {
        let policy = &self.policy_anchor;
        let target = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let is_policy = target == *policy
            || std::fs::canonicalize(&target)
                .ok()
                .zip(std::fs::canonicalize(policy).ok())
                .is_some_and(|(target, policy)| target == policy);
        if is_policy {
            return Err(".e-agent/config.toml controls sandbox and file capabilities; it must be modified by the user outside the agent".into());
        }
        Ok(())
    }

    fn external<'a, 'b>(
        &'a self,
        path: &'b Path,
        writable: bool,
    ) -> Result<(&'a ExternalRoot, &'b Path), String> {
        if path.components().any(|c| {
            !matches!(
                c,
                Component::RootDir | Component::Prefix(_) | Component::Normal(_)
            )
        }) {
            return Err("absolute external path must contain only normal components".into());
        }
        self.external
            .iter()
            .filter(|root| !writable || root.writable)
            .filter_map(|root| path.strip_prefix(&root.path).ok().map(|rest| (root, rest)))
            .max_by_key(|(root, _)| root.path.components().count())
            .ok_or_else(|| {
                if writable {
                    "absolute path is not within an authorized writable external root".into()
                } else {
                    "absolute path is not within an authorized external root".into()
                }
            })
    }

    fn relative<'a>(&self, input: &'a str) -> Result<&'a Path, String> {
        let path = Path::new(input);
        if path.as_os_str().is_empty()
            || path.is_absolute()
            || path
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(
                "path must be a non-empty workspace-relative path with only normal components"
                    .into(),
            );
        }
        Ok(path)
    }
}

#[cfg(target_os = "linux")]
fn secure_dir_write(dir: &Dir, path: &Path, content: &[u8]) -> Result<(), String> {
    use rustix::fs::{Mode, OFlags, ResolveFlags, mkdirat, openat2};

    let components: Vec<_> = path
        .components()
        .map(|component| match component {
            Component::Normal(name) => Ok(name),
            _ => Err("write failed: path must contain only normal components".to_owned()),
        })
        .collect::<Result<_, _>>()?;
    let (file_name, parents) = components
        .split_last()
        .ok_or("write failed: path must name a file")?;
    let mut parent = dir
        .try_clone()
        .map_err(|error| format!("write failed: {error}"))?;
    for name in parents {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let resolve = ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS;
        let opened = match openat2(&parent, *name, flags, Mode::empty(), resolve) {
            Ok(fd) => fd,
            Err(error) if error == rustix::io::Errno::NOENT => {
                mkdirat(&parent, *name, Mode::RWXU)
                    .map_err(|error| format!("create directory failed: {error}"))?;
                openat2(&parent, *name, flags, Mode::empty(), resolve)
                    .map_err(|error| format!("create directory failed: {error}"))?
            }
            Err(error) => return Err(format!("write failed: {error}")),
        };
        parent = Dir::from(opened);
    }
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::CLOEXEC;
    let fd = openat2(
        &parent,
        *file_name,
        flags,
        Mode::from(0o666),
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS,
    )
    .map_err(|error| format!("write failed: {error}"))?;
    let mut file = File::from(fd);
    file.write_all(content)
        .and_then(|()| file.flush())
        .map_err(|error| format!("write failed: {error}"))
}

#[cfg(not(target_os = "linux"))]
fn secure_dir_write(_dir: &Dir, _path: &Path, _content: &[u8]) -> Result<(), String> {
    Err("write failed: secure directory writes require Linux".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn rejects_parent_absolute_and_current_directory_paths() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(temp.path()).unwrap();
        assert!(workspace.write("../outside", "no").is_err());
        assert!(workspace.write("/tmp/outside", "no").is_err());
        assert!(workspace.write("./file", "no").is_err());
    }

    #[test]
    fn external_directories_and_exact_files_enforce_permissions() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_dir = temp.path().join("workspace");
        let readable = temp.path().join("readable");
        let writable = temp.path().join("writable");
        fs::create_dir_all(&workspace_dir).unwrap();
        fs::create_dir_all(&readable).unwrap();
        fs::create_dir_all(&writable).unwrap();
        fs::write(readable.join("input"), "read").unwrap();
        let exact = temp.path().join("exact");
        fs::write(&exact, "old").unwrap();
        let policy = crate::config::Sandbox {
            readable_paths: vec![readable.to_string_lossy().into_owned()],
            writable_paths: vec![
                writable.to_string_lossy().into_owned(),
                exact.to_string_lossy().into_owned(),
            ],
            ..Default::default()
        };
        let workspace = Workspace::new(&workspace_dir)
            .unwrap()
            .with_external_roots(&policy)
            .unwrap();
        assert_eq!(
            workspace
                .read(readable.join("input").to_str().unwrap())
                .unwrap(),
            b"read"
        );
        assert!(
            workspace
                .write(readable.join("input").to_str().unwrap(), "no")
                .is_err()
        );
        workspace
            .write(writable.join("new/leaf").to_str().unwrap(), "new")
            .unwrap();
        assert_eq!(fs::read(writable.join("new/leaf")).unwrap(), b"new");
        workspace.write(exact.to_str().unwrap(), "changed").unwrap();
        assert_eq!(fs::read(&exact).unwrap(), b"changed");
        assert!(
            workspace
                .read(temp.path().join("sibling").to_str().unwrap())
                .is_err()
        );
    }

    #[test]
    fn policy_file_is_readable_but_not_writable() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join(".e-agent")).unwrap();
        fs::write(temp.path().join(".e-agent/config.toml"), "[sandbox]\n").unwrap();
        let workspace = Workspace::new(temp.path()).unwrap();
        assert_eq!(
            workspace.read_to_string(".e-agent/config.toml").unwrap(),
            "[sandbox]\n"
        );
        let error = workspace.write(".e-agent/config.toml", "no").unwrap_err();
        assert!(error.contains("controls sandbox and file capabilities"));
    }

    #[test]
    fn exact_file_handle_is_serialized_across_clones() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_dir = temp.path().join("workspace");
        fs::create_dir(&workspace_dir).unwrap();
        let exact = temp.path().join("exact");
        let initial = vec![b'x'; 32 * 1024];
        fs::write(&exact, &initial).unwrap();
        let policy = crate::config::Sandbox {
            writable_paths: vec![exact.to_str().unwrap().to_owned()],
            ..Default::default()
        };
        let workspace = Workspace::new(&workspace_dir)
            .unwrap()
            .with_external_roots(&policy)
            .unwrap();
        let mut readers = Vec::new();
        for _ in 0..8 {
            let workspace = workspace.clone();
            let path = exact.to_str().unwrap().to_owned();
            let expected = initial.clone();
            readers.push(std::thread::spawn(move || {
                for _ in 0..50 {
                    assert_eq!(workspace.read(&path).unwrap(), expected);
                }
            }));
        }
        for reader in readers {
            reader.join().unwrap();
        }

        let one = vec![b'a'; 20_001];
        let two = vec![b'b'; 30_003];
        let first = workspace.clone();
        let first_path = exact.to_str().unwrap().to_owned();
        let first_payload = one.clone();
        let t1 = std::thread::spawn(move || first.write(&first_path, first_payload).unwrap());
        let second = workspace.clone();
        let second_path = exact.to_str().unwrap().to_owned();
        let second_payload = two.clone();
        let t2 = std::thread::spawn(move || second.write(&second_path, second_payload).unwrap());
        t1.join().unwrap();
        t2.join().unwrap();
        let final_content = fs::read(&exact).unwrap();
        assert!(final_content == one || final_content == two);
    }

    #[test]
    fn reroot_preserves_external_roots_and_ignores_local_policy() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("parent");
        let custom = parent.join("custom");
        let external = temp.path().join("external");
        fs::create_dir_all(custom.join(".e-agent")).unwrap();
        fs::create_dir(&external).unwrap();
        fs::write(external.join("visible"), "yes").unwrap();
        fs::write(
            custom.join(".e-agent/config.toml"),
            "[sandbox]\nreadable_paths = [\"/unauthorized\"]\n",
        )
        .unwrap();
        let policy = crate::config::Sandbox {
            readable_paths: vec![external.to_str().unwrap().to_owned()],
            ..Default::default()
        };
        let parent = Workspace::new(&parent)
            .unwrap()
            .with_external_roots(&policy)
            .unwrap();
        let custom = parent.reroot(&custom).unwrap();
        assert_eq!(
            custom
                .read(external.join("visible").to_str().unwrap())
                .unwrap(),
            b"yes"
        );
        assert!(custom.read("/unauthorized").is_err());
    }

    #[test]
    fn reroot_is_limited_to_existing_writable_authority() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_dir = temp.path().join("workspace");
        let child = workspace_dir.join("child");
        let writable = temp.path().join("writable");
        let readable = temp.path().join("readable");
        fs::create_dir_all(&child).unwrap();
        fs::create_dir(&writable).unwrap();
        fs::create_dir(&readable).unwrap();
        let workspace = Workspace::new(&workspace_dir)
            .unwrap()
            .with_external_roots(&crate::config::Sandbox {
                writable_paths: vec![writable.to_string_lossy().into_owned()],
                readable_paths: vec![readable.to_string_lossy().into_owned()],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(workspace.reroot(&child).unwrap().root(), child);
        assert_eq!(workspace.reroot(&writable).unwrap().root(), writable);
        assert!(workspace.reroot(&readable).is_err());
        assert!(workspace.reroot(temp.path()).is_err());
        assert!(workspace.reroot("/").is_err());
    }

    #[test]
    fn reroot_keeps_startup_policy_anchor_protected() {
        let temp = tempfile::tempdir().unwrap();
        let workspace_dir = temp.path().join("workspace");
        let child = workspace_dir.join("child");
        let policy = workspace_dir.join(".e-agent/config.toml");
        fs::create_dir_all(&child).unwrap();
        fs::create_dir_all(policy.parent().unwrap()).unwrap();
        fs::write(&policy, "[sandbox]\n").unwrap();
        let workspace = Workspace::new(&workspace_dir)
            .unwrap()
            .with_external_roots(&crate::config::Sandbox {
                writable_paths: vec![workspace_dir.to_string_lossy().into_owned()],
                ..Default::default()
            })
            .unwrap()
            .reroot(&child)
            .unwrap();
        assert_eq!(workspace.policy_anchor(), policy);
        assert!(workspace.write(policy.to_str().unwrap(), "no").is_err());
        assert_eq!(fs::read_to_string(policy).unwrap(), "[sandbox]\n");
    }

    #[cfg(unix)]
    #[test]
    fn policy_write_rejects_dangling_and_parent_symlink_aliases() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let policy_dir = workspace.join(".e-agent");
        fs::create_dir_all(&policy_dir).unwrap();
        let policy = policy_dir.join("config.toml");
        let dangling = workspace.join("dangling");
        symlink(".e-agent/config.toml", &dangling).unwrap();
        let parent_alias = workspace.join("policy-dir-alias");
        symlink(".e-agent", &parent_alias).unwrap();
        let workspace = Workspace::new(&workspace).unwrap();
        assert!(workspace.write("dangling", "no").is_err());
        assert!(
            workspace
                .write("policy-dir-alias/config.toml", "no")
                .is_err()
        );
        assert!(!policy.exists());
    }

    #[cfg(unix)]
    #[test]
    fn external_directory_allows_internal_symlink_but_rejects_escape() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let workspace_dir = temp.path().join("workspace");
        let external = temp.path().join("external");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&workspace_dir).unwrap();
        fs::create_dir_all(external.join("inside")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(external.join("inside/file"), "yes").unwrap();
        fs::write(outside.join("secret"), "no").unwrap();
        symlink("inside", external.join("link-in")).unwrap();
        symlink(&outside, external.join("link-out")).unwrap();
        let policy = crate::config::Sandbox {
            readable_paths: vec![external.to_string_lossy().into_owned()],
            ..Default::default()
        };
        let workspace = Workspace::new(&workspace_dir)
            .unwrap()
            .with_external_roots(&policy)
            .unwrap();
        assert_eq!(
            workspace
                .read(external.join("link-in/file").to_str().unwrap())
                .unwrap(),
            b"yes"
        );
        assert!(
            workspace
                .read(external.join("link-out/secret").to_str().unwrap())
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escapes_including_dangling_final_write() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "no").unwrap();
        symlink(outside.path(), temp.path().join("escape")).unwrap();
        let dangling_target = outside.path().join("must-not-exist");
        symlink(&dangling_target, temp.path().join("dangling")).unwrap();
        let workspace = Workspace::new(temp.path()).unwrap();
        assert!(workspace.read("escape/secret").is_err());
        assert!(workspace.write("escape/new", "no").is_err());
        assert!(workspace.write("dangling", "no").is_err());
        assert!(!dangling_target.exists());
    }
}

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use cap_std::ambient_authority;
use cap_std::fs::{Dir, File, OpenOptions};

/// The capability-relative workspace every file tool resolves against.
///
/// The workspace root is a dir capability; configured external roots are
/// opened from their **canonical source** (the security boundary) but are
/// looked up by their **configured logical destination** — the
/// `~`-expanded / workspace-joined form the user wrote, before
/// canonicalization — so a configured alias (e.g. a `~/.cargo` symlink
/// onto a canonical root) is addressable by file tools at the path the
/// user configured. Lookup picks the single most-specific logical winner
/// among ALL entries (workspace + external), then checks the winner's mode:
/// a writable operation against a most-specific read-only winner is
/// rejected outright and never falls back to a broader writable parent;
/// equal destinations resolve to the workspace entry.
///
/// When the `[sandbox]` is enabled, the workspace itself becomes a logical
/// policy entry whose writable flag is `workspace_writable`: with
/// `workspace_writable = false` reads stay allowed while writes, edits and
/// removes are denied, and an explicit writable child entry still wins by
/// specificity. With the sandbox disabled the workspace keeps the
/// historical always-writable behavior.
#[derive(Clone, Debug)]
pub struct Workspace {
    root: PathBuf,
    dir: Arc<Dir>,
    /// The startup policy file; rerooting never changes this anchor.
    policy_anchor: PathBuf,
    external: Arc<Vec<ExternalRoot>>,
    /// Whether the sandbox is enabled: only then does the workspace become
    /// a logical policy entry enforcing `workspace_writable`.
    sandbox_enabled: bool,
    /// The workspace entry's writable flag. Meaningful when the sandbox is
    /// enabled; with it disabled the workspace stays historically writable.
    workspace_writable: bool,
}

/// One logical external entry: the capability is opened from the canonical
/// `source`, while `dest` is the configured logical destination used for
/// lookup. The same canonical source can appear in several entries with
/// different destinations and writability (e.g. canonical RW + alias RO of
/// the same source coexist independently).
#[derive(Debug)]
struct ExternalRoot {
    /// Canonical path the capability was opened from — the security
    /// boundary used by reroot and policy-anchor visibility. Aliases never
    /// extend reroot: only the canonical source authorizes a custom
    /// workspace.
    source: PathBuf,
    /// Configured logical destination used for lookup — the
    /// `~`-expanded / workspace-joined form the user wrote, before
    /// canonicalization. Lookup matches this path lexically; the input is
    /// never ambiently canonicalized.
    dest: PathBuf,
    writable: bool,
    capability: ExternalCapability,
}

#[derive(Debug)]
enum ExternalCapability {
    Dir(Arc<Dir>),
    File(Arc<Mutex<File>>),
}

/// The resolved target of a user path: which logical entry covers it and
/// the remainder relative to that entry's root (empty = the entry root
/// itself).
enum Resolved<'a> {
    /// The workspace itself, addressed through its dir capability.
    Workspace { remainder: PathBuf },
    /// An external entry.
    External {
        root: &'a ExternalRoot,
        remainder: PathBuf,
    },
}

impl Workspace {
    pub fn new(root: impl AsRef<Path>) -> Result<Self, String> {
        // Canonicalize first — keeping the verbatim-prefixed form for the
        // open keeps >260-char Windows workspaces openable — then store the
        // prefix-stripped path so comparisons and `strip_prefix` against
        // ordinary user-provided paths work on Windows.
        let canonical = std::fs::canonicalize(root.as_ref())
            .map_err(|error| format!("cannot canonicalize workspace: {error}"))?;
        let dir = Dir::open_ambient_dir(&canonical, ambient_authority())
            .map_err(|error| format!("cannot open workspace: {error}"))?;
        let root = crate::strip_verbatim_prefix(&canonical);
        Ok(Self {
            policy_anchor: root.join(".e-agent/config.toml"),
            root,
            dir: Arc::new(dir),
            external: Arc::new(Vec::new()),
            sandbox_enabled: false,
            workspace_writable: true,
        })
    }

    /// Install a resolved policy. Ambient authority is used only once here to
    /// turn canonical configured roots into durable capabilities.
    ///
    /// Every canonical root in `writable_paths` / `readable_paths` becomes a
    /// self entry (dest == canonical source). In addition, every configured
    /// mount whose dest differs from its canonical source becomes an
    /// independent alias entry at the configured logical destination, so the
    /// file tools address a configured alias exactly where the user wrote it
    /// while the capability stays rooted at the canonical source.
    pub fn with_external_roots(mut self, sandbox: &crate::config::Sandbox) -> Result<Self, String> {
        let mut roots = Vec::new();
        for (writable, paths) in [
            (true, &sandbox.writable_paths),
            (false, &sandbox.readable_paths),
        ] {
            for path in paths {
                let source = PathBuf::from(path);
                push_external_root(&mut roots, source.clone(), source, writable)?;
            }
        }
        // Configured aliases: a (canonical source, configured dest) mount
        // whose dest differs from the canonical self-mount is an independent
        // logical entry. The same canonical source can therefore exist as
        // canonical RW and alias RO at the same time.
        for (writable, mounts) in [
            (true, &sandbox.writable_mounts),
            (false, &sandbox.readable_mounts),
        ] {
            for (source, dest) in mounts {
                let source = PathBuf::from(source);
                let dest = PathBuf::from(dest);
                if dest == source {
                    continue; // canonical self-mount, already covered above
                }
                push_external_root(&mut roots, source, dest, writable)?;
            }
        }
        self.external = Arc::new(roots);
        self.sandbox_enabled = sandbox.enabled;
        self.workspace_writable = sandbox.workspace_writable;
        Ok(self)
    }

    /// Derive a workspace only from authority already held by this workspace.
    /// The authority winner is the most specific canonical source covering
    /// the requested path — workspace and external entries all compete, and
    /// equal sources resolve to the workspace — and only a writable winning
    /// source reroots. A read-only workspace therefore rejects reroots into
    /// itself unless an explicit writable canonical directory child more
    /// specific than the workspace covers the path; external ancestors or
    /// equals never override it. A most-specific read-only or exact-file
    /// winning source rejects without falling back to a broader writable
    /// source; configured alias destinations never participate. With the
    /// sandbox disabled (or a writable workspace) the workspace stays
    /// writable provenance, unchanged.
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
        let Some((base, dir, writable)) = self.reroot_authority(requested) else {
            return Err("custom workspace is not within the workspace or an authorized writable external directory".into());
        };
        if !writable {
            return Err("custom workspace is not within the workspace or an authorized writable external directory".into());
        }
        let remainder = requested
            .strip_prefix(&base)
            .expect("reroot authority covers the requested path");
        self.reroot_from(&base, &dir, remainder, writable)
    }

    /// Pick the reroot authority winner for `requested`: the most specific
    /// canonical source covering it among the workspace and every external
    /// entry, never first-match. Equal sources resolve to the workspace —
    /// an external equal or ancestor can never upgrade a read-only
    /// workspace — so a path inside a read-only workspace is only rerootable
    /// through a more specific writable canonical directory child. External
    /// sources compete by specificity alone (read-only and exact-file
    /// entries included): the winning source's effective write authority
    /// decides, so a most-specific read-only or exact-file source rejects
    /// without falling back to a broader writable source. Alias
    /// destinations never participate; only canonical sources match.
    /// Returns `(source, dir capability, writable)`.
    fn reroot_authority(&self, requested: &Path) -> Option<(PathBuf, Arc<Dir>, bool)> {
        let mut winner: Option<(usize, PathBuf, Option<Arc<Dir>>, bool)> = None;
        if requested.strip_prefix(&self.root).is_ok() {
            winner = Some((
                self.root.components().count(),
                self.root.clone(),
                Some(self.dir.clone()),
                !self.sandbox_enabled || self.workspace_writable,
            ));
        }
        for root in self.external.iter() {
            if requested.strip_prefix(&root.source).is_err() {
                continue;
            }
            let specificity = root.source.components().count();
            let dir = match &root.capability {
                ExternalCapability::Dir(dir) => Some(dir.clone()),
                ExternalCapability::File(_) => None,
            };
            match &mut winner {
                None => {
                    let writable = root.writable && dir.is_some();
                    winner = Some((specificity, root.source.clone(), dir, writable));
                }
                Some((best, best_source, best_dir, best_writable)) => {
                    if specificity < *best {
                        continue;
                    }
                    if specificity == *best {
                        // Equal specificity ⇒ identical sources. The
                        // workspace wins its own source outright; equal
                        // external sources merge their write authority.
                        if *best_source == root.source && *best_source != self.root && root.writable
                        {
                            *best_dir = dir;
                            *best_writable = true;
                        }
                        continue;
                    }
                    let writable = root.writable && dir.is_some();
                    *best = specificity;
                    *best_source = root.source.clone();
                    *best_dir = dir;
                    *best_writable = writable;
                }
            }
        }
        match winner {
            Some((_, source, Some(dir), writable)) => Some((source, dir, writable)),
            // No coverage, or the most-specific source is an exact file:
            // no directory authority to reroot from.
            _ => None,
        }
    }

    fn reroot_from(
        &self,
        base: &Path,
        dir: &Arc<Dir>,
        remainder: &Path,
        workspace_writable: bool,
    ) -> Result<Self, String> {
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
            sandbox_enabled: self.sandbox_enabled,
            workspace_writable,
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
                .any(|root| self.policy_anchor.starts_with(&root.source))
    }

    pub fn read(&self, input: &str) -> Result<Vec<u8>, String> {
        match self.resolve_path(input, false)? {
            Resolved::Workspace { remainder } => self
                .dir
                .read(&remainder)
                .map_err(|error| format!("read failed: {error}")),
            Resolved::External { root, remainder } => match &root.capability {
                ExternalCapability::Dir(dir) => dir
                    .read(&remainder)
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
            },
        }
    }

    pub fn read_to_string(&self, input: &str) -> Result<String, String> {
        String::from_utf8(self.read(input)?).map_err(|error| format!("read failed: {error}"))
    }

    /// Read a file as UTF-8, mapping a missing file to `None` — callers can
    /// distinguish "the file did not exist" from other read failures (used
    /// by write_file's undo snapshot, where `None` means undo = delete).
    pub fn try_read_to_string(&self, input: &str) -> Result<Option<String>, String> {
        let exists = match self.resolve_path(input, false)? {
            Resolved::Workspace { remainder } => self
                .dir
                .try_exists(&remainder)
                .map_err(|error| format!("stat failed: {error}"))?,
            Resolved::External { root, remainder } => match &root.capability {
                ExternalCapability::Dir(dir) => dir
                    .try_exists(&remainder)
                    .map_err(|error| format!("stat failed: {error}"))?,
                // The capability is an open file handle; it exists by
                // definition (opened at startup).
                ExternalCapability::File(_) if remainder.as_os_str().is_empty() => true,
                ExternalCapability::File(_) => {
                    return Err("external file capability does not authorize sibling paths".into());
                }
            },
        };
        if !exists {
            return Ok(None);
        }
        self.read_to_string(input).map(Some)
    }

    /// Delete a file (used by undo for a `write_file` that created it).
    pub fn remove_file(&self, input: &str) -> Result<(), String> {
        match self.resolve_path(input, true)? {
            Resolved::Workspace { remainder } => self
                .dir
                .remove_file(&remainder)
                .map_err(|error| format!("delete failed: {error}")),
            Resolved::External { root, remainder } => match &root.capability {
                ExternalCapability::Dir(dir) => dir
                    .remove_file(&remainder)
                    .map_err(|error| format!("delete failed: {error}")),
                ExternalCapability::File(_) => {
                    Err("external file capability does not authorize deletion".into())
                }
            },
        }
    }

    pub fn write(&self, input: &str, content: impl AsRef<[u8]>) -> Result<(), String> {
        let path = Path::new(input);
        self.reject_policy_write(path)?;
        match self.resolve_path(input, true)? {
            Resolved::Workspace { remainder } => {
                secure_dir_write(&self.dir, &remainder, content.as_ref())
            }
            Resolved::External { root, remainder } => match &root.capability {
                ExternalCapability::Dir(dir) => secure_dir_write(dir, &remainder, content.as_ref()),
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
            },
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
            || crate::canonicalize_path(&target)
                .ok()
                .zip(crate::canonicalize_path(policy).ok())
                .is_some_and(|(target, policy)| target == policy);
        if is_policy {
            return Err(".e-agent/config.toml controls sandbox and file capabilities; it must be modified by the user outside the agent".into());
        }
        Ok(())
    }

    /// The unified capability resolver behind read / try-read / write /
    /// edit / remove. Relative paths join the workspace root and resolve
    /// like absolute ones, so the most-specific logical entry — including
    /// an explicit external child inside the workspace — wins for them
    /// too. `writable` selects the operation's mode, enforced against the
    /// single most-specific winner (reads and writes pick the same winner).
    fn resolve_path(&self, input: &str, writable: bool) -> Result<Resolved<'_>, String> {
        let path = Path::new(input);
        if path.is_absolute() {
            return self.resolve_absolute(&crate::strip_verbatim_prefix(path), writable);
        }
        let relative = self.relative(input)?;
        let absolute = self.root.join(relative);
        self.resolve_absolute(&absolute, writable)
    }

    /// Resolve an absolute path against the logical entries: the workspace
    /// slot plus every external entry. Lookup matches the configured
    /// logical destination lexically — the input is never ambiently
    /// canonicalized — and picks the single most-specific winner among ALL
    /// entries, with equal destinations resolving to the workspace entry.
    /// Only AFTER the winner is chosen is its mode checked: a writable
    /// operation against a most-specific read-only winner is rejected
    /// outright and never falls back to a broader writable parent. Reads
    /// select the same winner. The workspace is a logical policy entry when
    /// the sandbox is enabled (`writable = workspace_writable`); with the
    /// sandbox disabled it keeps the historical always-writable behavior.
    fn resolve_absolute<'a>(
        &'a self,
        absolute: &Path,
        writable: bool,
    ) -> Result<Resolved<'a>, String> {
        if absolute.components().any(|part| {
            !matches!(
                part,
                Component::RootDir | Component::Prefix(_) | Component::Normal(_)
            )
        }) {
            return Err("absolute external path must contain only normal components".into());
        }
        let workspace_writable = !self.sandbox_enabled || self.workspace_writable;
        let mut best: Option<(usize, Resolved<'a>)> = None;
        if let Ok(remainder) = absolute.strip_prefix(&self.root)
            && remainder
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
        {
            best = Some((
                self.root.components().count(),
                Resolved::Workspace {
                    remainder: remainder.to_path_buf(),
                },
            ));
        }
        for root in self.external.iter() {
            if let Ok(remainder) = absolute.strip_prefix(&root.dest)
                && best
                    .as_ref()
                    .is_none_or(|(specificity, _)| root.dest.components().count() > *specificity)
            {
                best = Some((
                    root.dest.components().count(),
                    Resolved::External {
                        root,
                        remainder: remainder.to_path_buf(),
                    },
                ));
            }
        }
        match best {
            Some((_, resolved)) => {
                if writable {
                    match &resolved {
                        Resolved::Workspace { .. } if !workspace_writable => {
                            return Err(
                                "workspace is read-only: sandbox `workspace_writable = false` denies file-tool writes"
                                    .into(),
                            );
                        }
                        Resolved::External { root, .. } if !root.writable => {
                            // The most-specific winner is read-only: reject
                            // directly — never fall back to a broader
                            // writable parent.
                            return Err(
                                "absolute path is within a read-only external root; writable operations are denied"
                                    .into(),
                            );
                        }
                        _ => {}
                    }
                }
                Ok(resolved)
            }
            None => Err(if writable {
                "absolute path is not within an authorized writable external root".into()
            } else {
                "absolute path is not within an authorized external root".into()
            }),
        }
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

/// Open one logical external entry from its canonical source and append it
/// unless an identical (source, dest, writable) entry already exists.
fn push_external_root(
    roots: &mut Vec<ExternalRoot>,
    source: PathBuf,
    dest: PathBuf,
    writable: bool,
) -> Result<(), String> {
    if roots
        .iter()
        .any(|root| root.source == source && root.dest == dest && root.writable == writable)
    {
        return Ok(());
    }
    let metadata = std::fs::metadata(&source)
        .map_err(|error| format!("cannot inspect external root {}: {error}", source.display()))?;
    let capability = if metadata.is_dir() {
        ExternalCapability::Dir(Arc::new(
            Dir::open_ambient_dir(&source, ambient_authority()).map_err(|error| {
                format!(
                    "cannot open external directory {}: {error}",
                    source.display()
                )
            })?,
        ))
    } else {
        let mut options = OpenOptions::new();
        options.read(true).write(writable);
        ExternalCapability::File(Arc::new(Mutex::new(
            File::open_ambient_with(&source, &options, ambient_authority()).map_err(|error| {
                format!("cannot open external file {}: {error}", source.display())
            })?,
        )))
    };
    roots.push(ExternalRoot {
        source,
        dest,
        writable,
        capability,
    });
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
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

#[cfg(windows)]
fn secure_dir_write(dir: &Dir, path: &Path, content: &[u8]) -> Result<(), String> {
    // Degraded write for Windows only (openat2 is Linux-only): fall back to
    // cap-std's regular capability-relative open/truncate/write. The
    // BENEATH|NO_SYMLINKS resolution guarantees of the Linux path are lost
    // (tree-internal symlinks are followed but cannot escape the capability
    // root — cap-std resolves components with FollowSymlinks::No and rejects
    // absolute/escaping targets), but `path` is still validated to contain
    // only normal components by the caller's `relative()`/`external()`, and
    // `dir` is the capability root, so the write cannot escape the
    // authorized directory. Other non-Linux platforms (macOS etc.) keep the
    // fail-closed error below — the C plan only covers Windows.
    // Keeps the same signature and error type as the Linux path.
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        dir.create_dir_all(parent)
            .map_err(|error| format!("create directory failed: {error}"))?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    let mut file = dir
        .open_with(path, &options)
        .map_err(|error| format!("write failed: {error}"))?;
    file.write_all(content)
        .and_then(|()| file.flush())
        .map_err(|error| format!("write failed: {error}"))
}

#[cfg(not(any(target_os = "linux", target_os = "android", windows)))]
fn secure_dir_write(_dir: &Dir, _path: &Path, _content: &[u8]) -> Result<(), String> {
    // Other non-Linux platforms (macOS, BSD, …) keep the original
    // fail-closed behavior: secure directory writes require Linux's
    // openat2. The C plan deliberately covers Windows only.
    Err("write failed: secure directory writes require Linux".into())
}

#[cfg(test)]
mod tests;

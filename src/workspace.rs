use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use cap_std::ambient_authority;
use cap_std::fs::Dir;

#[derive(Clone, Debug)]
pub struct Workspace {
    root: PathBuf,
    dir: Arc<Dir>,
}

impl Workspace {
    pub fn new(root: impl AsRef<Path>) -> Result<Self, String> {
        let root = std::fs::canonicalize(root.as_ref())
            .map_err(|error| format!("cannot canonicalize workspace: {error}"))?;
        let dir = Dir::open_ambient_dir(&root, ambient_authority())
            .map_err(|error| format!("cannot open workspace: {error}"))?;
        Ok(Self {
            root,
            dir: Arc::new(dir),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn read(&self, input: &str) -> Result<Vec<u8>, String> {
        self.dir
            .read(self.relative(input)?)
            .map_err(|error| format!("read failed: {error}"))
    }

    pub fn read_to_string(&self, input: &str) -> Result<String, String> {
        self.dir
            .read_to_string(self.relative(input)?)
            .map_err(|error| format!("read failed: {error}"))
    }

    pub fn write(&self, input: &str, content: impl AsRef<[u8]>) -> Result<(), String> {
        let path = self.relative(input)?;
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            self.dir
                .create_dir_all(parent)
                .map_err(|error| format!("create directory failed: {error}"))?;
        }
        self.dir
            .write(path, content)
            .map_err(|error| format!("write failed: {error}"))
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

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn rejects_parent_absolute_and_current_directory_paths() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(temp.path()).unwrap();
        assert!(workspace.write("../outside", "no").is_err());
        assert!(workspace.write("/tmp/outside", "no").is_err());
        assert!(workspace.write("./file", "no").is_err());
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

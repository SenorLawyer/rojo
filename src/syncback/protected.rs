//! The partitions syncback leaves alone: the `$path` of every project node
//! with `$globIgnorePaths`. The files those globs exclude are not in the tree
//! syncback compares, so writing, removing or replacing anything there could
//! overwrite or delete them.

use std::path::{Component, Path, PathBuf};

/// Protected partition folders, compared by where they are on disk rather
/// than by how a project or syncback spelled their paths, so `src/../shared`
/// and a link to `shared` are both `shared`.
#[derive(Debug, Default)]
pub struct ProtectedPartitions {
    /// A description of each node for warnings, and its canonical folder.
    bases: Vec<(String, PathBuf)>,
}

impl ProtectedPartitions {
    /// Adds the partition at `base`, unless one at the same place is listed.
    /// Returns whether it was added.
    pub fn add(&mut self, node: String, base: &Path) -> bool {
        let base = canonical(base);
        if self.bases.iter().any(|(_, known)| same(known, &base)) {
            return false;
        }
        self.bases.push((node, base));
        true
    }

    pub fn iter(&self) -> impl Iterator<Item = &(String, PathBuf)> {
        self.bases.iter()
    }

    /// Whether `path` is a partition or inside one.
    pub fn contains(&self, path: &Path) -> bool {
        if self.bases.is_empty() {
            return false;
        }
        let path = canonical(path);
        self.bases.iter().any(|(_, base)| starts_with(&path, base))
    }

    /// Whether removing or replacing `path` would remove a partition, because
    /// it is one or holds one.
    pub fn held_by(&self, path: &Path) -> bool {
        if self.bases.is_empty() {
            return false;
        }
        let path = canonical(path);
        self.bases.iter().any(|(_, base)| starts_with(base, &path))
    }
}

/// `path` with its nearest existing ancestor resolved on disk (links and
/// `..` included) and the components that do not exist yet appended, with
/// `.` and `..` among those applied lexically.
pub fn canonical(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_owned());
    let mut rest = Vec::new();
    let mut existing = absolute.as_path();
    let resolved = loop {
        if let Ok(resolved) = dunce::canonicalize(existing) {
            break resolved;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                existing = parent;
            }
            // Only `..` or a root is left that cannot be resolved; keep the
            // path as given.
            _ => return absolute,
        }
    };
    let mut out = resolved;
    for name in rest.into_iter().rev() {
        match Path::new(&name).components().next() {
            Some(Component::ParentDir) => {
                out.pop();
            }
            Some(Component::CurDir) | None => {}
            _ => out.push(name),
        }
    }
    out
}

/// Component-wise prefix test, ignoring case on Windows, whose file systems
/// usually do.
fn starts_with(path: &Path, base: &Path) -> bool {
    if cfg!(windows) {
        let lower = |path: &Path| PathBuf::from(path.to_string_lossy().to_lowercase());
        lower(path).starts_with(lower(base))
    } else {
        path.starts_with(base)
    }
}

fn same(a: &Path, b: &Path) -> bool {
    starts_with(a, b) && starts_with(b, a)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn aliases_of_a_partition_are_the_partition() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("shared/inner")).unwrap();

        let mut protected = ProtectedPartitions::default();
        assert!(protected.add("Shared".into(), &root.join("src/../shared")));
        assert!(!protected.add("Again".into(), &root.join("shared")));

        assert!(protected.contains(&root.join("shared/inner/Foo.luau")));
        assert!(protected.contains(&root.join("src/../shared/New.luau")));
        // Not created yet, and reached through `..` after it.
        assert!(protected.contains(&root.join("src/new/../../shared/x/Foo.luau")));
        assert!(!protected.contains(&root.join("src/Foo.luau")));
        assert!(!protected.contains(&root.join("sharedother/Foo.luau")));

        assert!(protected.held_by(root));
        assert!(protected.held_by(&root.join("src/..")));
        assert!(protected.held_by(&root.join("shared")));
        assert!(!protected.held_by(&root.join("shared/inner")));
        assert!(!protected.held_by(&root.join("src")));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_a_partition_is_the_partition() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("shared")).unwrap();
        std::os::unix::fs::symlink(root.join("shared"), root.join("link")).unwrap();

        let mut protected = ProtectedPartitions::default();
        protected.add("Shared".into(), &root.join("shared"));
        assert!(protected.contains(&root.join("link/Foo.luau")));
        assert!(protected.held_by(&root.join("link")));

        let mut protected = ProtectedPartitions::default();
        protected.add("Link".into(), &root.join("link"));
        assert!(protected.contains(&root.join("shared/Foo.luau")));
    }
}

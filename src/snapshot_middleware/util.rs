use std::{
    io,
    path::{Path, PathBuf},
};

use anyhow::Context;
use memofs::Vfs;

/// If the given string ends up with the given suffix, returns the portion of
/// the string before the suffix.
pub fn match_trailing<'a>(input: &'a str, suffix: &str) -> Option<&'a str> {
    if input.ends_with(suffix) {
        let end = input.len().saturating_sub(suffix.len());
        Some(&input[..end])
    } else {
        None
    }
}

pub trait PathExt {
    fn file_name_ends_with(&self, suffix: &str) -> bool;
    fn file_name_trim_end<'a>(&'a self, suffix: &str) -> anyhow::Result<&'a str>;
    fn parent_err(&self) -> anyhow::Result<&Path>;
}

impl<P> PathExt for P
where
    P: AsRef<Path>,
{
    fn file_name_ends_with(&self, suffix: &str) -> bool {
        self.as_ref()
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.ends_with(suffix))
            .unwrap_or(false)
    }

    fn file_name_trim_end<'a>(&'a self, suffix: &str) -> anyhow::Result<&'a str> {
        let path = self.as_ref();
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .with_context(|| format!("Path did not have a file name: {}", path.display()))?;

        match_trailing(file_name, suffix)
            .with_context(|| format!("Path did not end in {}: {}", suffix, path.display()))
    }

    fn parent_err(&self) -> anyhow::Result<&Path> {
        let path = self.as_ref();
        path.parent()
            .with_context(|| format!("Path does not have a parent: {}", path.display()))
    }
}

// TEMP function until rojo 8.0, when it can be replaced with bool::default (aka false)
pub fn emit_legacy_scripts_default() -> Option<bool> {
    Some(true)
}

/// Canonicalizes the deepest existing ancestor of `path` and appends the
/// components below it, so that paths which do not exist (yet or anymore)
/// still use the same form as the canonical paths stored in the tree.
pub fn canonicalize_existing_ancestor(vfs: &Vfs, path: &Path) -> io::Result<PathBuf> {
    let mut ancestor = path;
    let mut suffix = Vec::new();

    loop {
        match vfs.canonicalize(ancestor) {
            Ok(mut canonical) => {
                for component in suffix.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                // A recursive delete can remove every parent before its first
                // child event is handled. Resolve the deepest surviving parent
                // so symlinked and Windows verbatim paths still match the tree.
                match (ancestor.parent(), ancestor.file_name()) {
                    (Some(parent), Some(name)) => {
                        suffix.push(name.to_owned());
                        ancestor = parent;
                    }
                    _ => return Err(err),
                }
            }
            Err(err) => return Err(err),
        }
    }
}

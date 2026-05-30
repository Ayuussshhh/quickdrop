//! Receiver-side file handling: category sorting, atomic rename,
//! duplicate-name resolution, and **path safety**.
//!
//! The two non-negotiable invariants here are:
//!
//! 1. A peer cannot write outside the destination root, no matter
//!    what `rel_path` they send. Any `..` segment, absolute path,
//!    drive letter, or Windows reserved name is rejected.
//! 2. We never overwrite an existing file. Conflicts are resolved by
//!    appending ` (n)` before the extension.

use std::path::{Component, Path, PathBuf};

use crate::{Error, Result};

/// Map a file extension to the category subfolder name.
/// Returns `None` for the catch-all "Other" bucket so callers can
/// handle that case explicitly.
pub fn category_for_ext(ext: &str) -> Option<&'static str> {
    let e = ext.to_ascii_lowercase();
    match e.as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "heic" | "heif" | "tiff" | "tif" => {
            Some("Images")
        }
        "mp4" | "mkv" | "mov" | "avi" | "webm" | "m4v" => Some("Videos"),
        "pdf" | "docx" | "xlsx" | "pptx" | "txt" | "md" | "odt" | "rtf" | "csv" => {
            Some("Documents")
        }
        "zip" | "7z" | "rar" | "tar" | "gz" | "tgz" | "bz2" | "xz" => Some("Archives"),
        _ => None,
    }
}

/// Validate a `rel_path` from the wire and return a safe relative
/// `PathBuf` that, when joined to a destination root, *cannot*
/// escape it.
pub fn sanitize_rel_path(raw: &str) -> Result<PathBuf> {
    if raw.is_empty() {
        return Err(Error::Protocol("empty rel_path".into()));
    }
    if raw.len() > 4096 {
        return Err(Error::Protocol("rel_path too long".into()));
    }
    if raw
        .chars()
        .any(|c| c == '\0' || (c.is_control() && c != '\t'))
    {
        return Err(Error::Protocol("rel_path has control chars".into()));
    }
    let normalised: String = raw.replace('\\', "/");
    let p = Path::new(&normalised);
    if p.is_absolute() {
        return Err(Error::Protocol("rel_path is absolute".into()));
    }

    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::Normal(s) => {
                let s = s
                    .to_str()
                    .ok_or_else(|| Error::Protocol("non-utf8 path component".into()))?;
                let cleaned = sanitize_segment(s)?;
                out.push(cleaned);
            }
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(Error::Protocol("rel_path contains ..".into()));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(Error::Protocol("rel_path has root/drive prefix".into()));
            }
        }
    }
    if out.as_os_str().is_empty() {
        return Err(Error::Protocol("rel_path resolves to empty".into()));
    }
    Ok(out)
}

fn sanitize_segment(seg: &str) -> Result<String> {
    if seg.is_empty() {
        return Err(Error::Protocol("empty path segment".into()));
    }
    let stem = seg.split('.').next().unwrap_or("").to_ascii_uppercase();
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
        "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if RESERVED.contains(&stem.as_str()) {
        return Err(Error::Protocol(format!(
            "rel_path uses reserved name: {seg}"
        )));
    }
    let mut cleaned: String = seg
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    while cleaned.ends_with('.') || cleaned.ends_with(' ') {
        cleaned.pop();
    }
    if cleaned.is_empty() {
        return Err(Error::Protocol("path segment empty after sanitising".into()));
    }
    Ok(cleaned)
}

/// Resolve the final destination directory for an item, applying
/// category sorting if enabled. Returns `(dir, file_name)`.
pub fn resolve_dest(
    root: &Path,
    rel: &Path,
    sort_by_category: bool,
) -> (PathBuf, std::ffi::OsString) {
    let file_name = rel
        .file_name()
        .map(|s| s.to_owned())
        .unwrap_or_else(|| std::ffi::OsString::from("file"));
    let parent = rel.parent().unwrap_or_else(|| Path::new(""));
    if !sort_by_category || !parent.as_os_str().is_empty() {
        return (root.join(parent), file_name);
    }
    let ext = rel.extension().and_then(|s| s.to_str()).unwrap_or("");
    let dir = match category_for_ext(ext) {
        Some(cat) => root.join(cat),
        None => root.join("Other"),
    };
    (dir, file_name)
}

/// If `dir/name` already exists, return `dir/name (1)`, then `(2)`, etc.
pub fn unique_dest(dir: &Path, name: &std::ffi::OsStr) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let name_str = name.to_string_lossy().to_string();
    let (stem, ext) = match name_str.rfind('.') {
        Some(i) if i > 0 => (&name_str[..i], &name_str[i..]),
        _ => (name_str.as_str(), ""),
    };
    for n in 1..10_000 {
        let trial = dir.join(format!("{stem} ({n}){ext}"));
        if !trial.exists() {
            return trial;
        }
    }
    dir.join(format!("{stem}-{}{ext}", uuid::Uuid::new_v4().simple()))
}

/// Atomically promote `src` (a `.qdpart` file) to `dst`.
pub fn finalize_part(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(src, dst)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal_and_absolutes() {
        assert!(sanitize_rel_path("../etc/passwd").is_err());
        assert!(sanitize_rel_path("/etc/passwd").is_err());
        assert!(sanitize_rel_path("C:/Windows/System32").is_err());
        assert!(sanitize_rel_path("a/../b").is_err());
        assert!(sanitize_rel_path("").is_err());
        assert!(sanitize_rel_path("a/\0/b").is_err());
    }

    #[test]
    fn rejects_reserved_windows_names() {
        assert!(sanitize_rel_path("CON").is_err());
        assert!(sanitize_rel_path("a/NUL.txt").is_err());
        assert!(sanitize_rel_path("LPT1").is_err());
    }

    #[test]
    fn cleans_illegal_chars() {
        let p = sanitize_rel_path("docs/he<l>lo:world?.txt").unwrap();
        assert_eq!(p, PathBuf::from("docs").join("he_l_lo_world_.txt"));
    }

    #[test]
    fn back_slashes_are_normalised() {
        let p = sanitize_rel_path("a\\b\\c.txt").unwrap();
        assert_eq!(p, PathBuf::from("a").join("b").join("c.txt"));
    }

    #[test]
    fn resolve_dest_categories() {
        let root = Path::new("R");
        let (d, n) = resolve_dest(root, Path::new("photo.jpg"), true);
        assert_eq!(d, Path::new("R").join("Images"));
        assert_eq!(n, "photo.jpg");

        let (d, _) = resolve_dest(root, Path::new("blob.bin"), true);
        assert_eq!(d, Path::new("R").join("Other"));

        let (d, _) = resolve_dest(root, Path::new("trip/photo.jpg"), true);
        assert_eq!(d, Path::new("R").join("trip"));
    }

    #[test]
    fn unique_dest_resolves_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = unique_dest(dir.path(), std::ffi::OsStr::new("a.txt"));
        std::fs::write(&p1, b"x").unwrap();
        let p2 = unique_dest(dir.path(), std::ffi::OsStr::new("a.txt"));
        assert_ne!(p1, p2);
        assert!(p2.to_string_lossy().contains("(1)"));
    }
}

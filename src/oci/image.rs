//! Deterministic OCI/Docker image flattening.
//!
//! Container images carry host-specific metadata — creation timestamps,
//! ownership, setuid bits — that leaks nondeterminism into a fresh run. This
//! module flattens an OCI/Docker layer tarball into a rootfs with normalized
//! metadata: every file gets mtime 0, a canonical mode, ownership of 0:0 where
//! permitted, and the whiteout semantics of a Docker layer are honoured.

use flate2::read::GzDecoder;
use std::fs;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use tracing::warn;

/// Errors from image flattening.
#[derive(Debug)]
pub enum ImageError {
    Io(std::io::Error),
    Tar(Box<dyn std::error::Error + Send + Sync>),
}

impl std::fmt::Display for ImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImageError::Io(e) => write!(f, "io error: {e}"),
            ImageError::Tar(e) => write!(f, "tar error: {e}"),
        }
    }
}

impl std::error::Error for ImageError {}

impl From<std::io::Error> for ImageError {
    fn from(value: std::io::Error) -> Self {
        ImageError::Io(value)
    }
}

/// Flatten an OCI/Docker image tarball into a deterministic rootfs at `dest`.
///
/// Gzip is detected by magic bytes; any other archive type is treated as a
/// plain tar. Existing contents of `dest` are preserved. Entries are streamed
/// in archive order so that Docker whiteouts (which must run after the file
/// they delete) keep their semantics; `normalize_tree` then canonicalizes all
/// metadata so the result is byte-reproducible regardless of the archive's
/// own timestamps or ownership.
pub fn flatten(image: &Path, dest: &Path) -> Result<(), ImageError> {
    if !dest.exists() {
        fs::create_dir_all(dest)?;
    }

    let mut file = fs::File::open(image)?;
    let mut magic = [0u8; 2];
    file.read_exact(&mut magic)?;
    let is_gzip = magic == [0x1f, 0x8b];
    file.seek(SeekFrom::Start(0))?;

    let reader = BufReader::new(file);
    if is_gzip {
        unpack_reader(GzDecoder::new(reader), dest)
    } else {
        unpack_reader(reader, dest)
    }
}

fn unpack_reader<R: Read>(reader: R, dest: &Path) -> Result<(), ImageError> {
    let mut archive = tar::Archive::new(reader);

    for entry in archive.entries()? {
        let mut entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let raw_path = match entry.path() {
            Ok(p) => p.into_owned(),
            Err(e) => {
                warn!(?e, "skipping entry with bad path");
                continue;
            }
        };

        let Some(safe) = sanitize_path(&raw_path) else {
            continue;
        };

        // Docker whiteouts: `.wh.<name>` deletes a file, `.wh..wh..opq`
        // clears a directory.
        if let Some(file_name) = safe.file_name().and_then(|n| n.to_str()) {
            if let Some(deleted) = file_name.strip_prefix(".wh.")
                && deleted == ".wh..opq"
            {
                let dir = safe.parent().unwrap_or(Path::new(""));
                let _ = fs::remove_dir_all(dest.join(dir));
                continue;
            }
            if let Some(deleted) = file_name.strip_prefix(".wh.") {
                let victim = safe.parent().unwrap_or(Path::new("")).join(deleted);
                let _ = fs::remove_file(dest.join(&victim));
                let _ = fs::remove_dir_all(dest.join(&victim));
                continue;
            }
        }

        entry
            .unpack_in(dest)
            .map_err(|e| ImageError::Tar(Box::new(e)))?;
    }

    normalize_tree(dest)
}

/// Normalize metadata across the extracted tree: epoch mtimes, canonical
/// modes, and best-effort 0:0 ownership.
fn normalize_tree(root: &Path) -> Result<(), ImageError> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let meta = entry.metadata()?;

            let _ = normalize_meta(&path, meta.is_dir());

            if meta.is_dir() {
                stack.push(path);
            }
        }
    }
    Ok(())
}

/// Set mtime 0, canonical mode, and best-effort ownership of 0:0.
fn normalize_meta(path: &Path, is_dir: bool) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let meta = fs::metadata(path)?;
    let mode = meta.mode();

    let normalized = if is_dir || mode & 0o111 != 0 {
        0o755
    } else {
        0o644
    };
    // Strip any suid/sgid/sticky bits, keeping only the normalized rwx bits.
    let _ = fs::set_permissions(
        path,
        std::os::unix::fs::PermissionsExt::from_mode(normalized as u32),
    );

    // Best effort: ownership is only writable for the container runtime.
    let c_path = match std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };
    unsafe {
        libc::chown(c_path.as_ptr(), 0, 0);
    }

    // Epoch mtime.
    let times = [libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    }; 2];
    unsafe {
        libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0);
    }
    Ok(())
}

/// Reject unsafe paths (absolute, `..`, drive letters) and return a relative
/// `PathBuf` anchored at the archive root.
fn sanitize_path(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() || path.as_os_str().is_empty() {
        return None;
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(c) => out.push(c),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

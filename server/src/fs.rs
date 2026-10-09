//! Cross-platform durable file publication.

use std::path::Path;

/// Atomically replaces `target` with `source` without removing `target` first.
/// If replacement fails, the previous target remains in place.
pub(crate) fn replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    #[cfg(not(windows))]
    {
        std::fs::rename(source, target)
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        };

        let source = source
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let target = target
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let replaced = unsafe {
            MoveFileExW(
                source.as_ptr(),
                target.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if replaced == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_replacement_preserves_existing_target() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target.json");
        std::fs::write(&target, "old").unwrap();

        assert!(replace_file(&root.path().join("missing"), &target).is_err());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "old");
    }

    #[test]
    fn replacement_overwrites_existing_target() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.json");
        let target = root.path().join("target.json");
        std::fs::write(&source, "new").unwrap();
        std::fs::write(&target, "old").unwrap();

        replace_file(&source, &target).unwrap();
        assert_eq!(std::fs::read_to_string(target).unwrap(), "new");
        assert!(!source.exists());
    }
}

//! The file layer.
//!
//! Windows is the release target, so `seek_write`, `FSCTL_SET_SPARSE`, `MoveFileExW` and the
//! Mark-of-the-Web are called directly. Every OS-specific call is confined to this module
//! behind a narrow signature; the Unix arms exist so the engine can be developed and tested
//! on a Linux CI machine, and are not a portability commitment.

use std::{
    fs::{File, OpenOptions},
    io,
    path::{Path, PathBuf},
};

pub mod motw;

/// Extra headroom demanded on top of the file size before a download starts, so the disk
/// does not fill to literally zero.
pub const FREE_SPACE_MARGIN: u64 = 100 * 1024 * 1024;

/// A partially downloaded file. Data lands here; the final name is only claimed at the end,
/// so a half-finished download can never be mistaken for a complete one.
pub fn part_path(dest_dir: &Path, filename: &str) -> PathBuf {
    dest_dir.join(format!("{filename}.slpart"))
}

/// Create or open the part file for writing, preallocating `total` bytes when known.
///
/// Preallocation is not an optimization detail: it makes a disk-full condition fail at the
/// start instead of at 90%, and it reduces NTFS fragmentation across a multi-GB file written
/// out of order.
pub fn open_part(path: &Path, total: Option<u64>) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new().create(true).read(true).write(true).open(path)?;

    if let Some(total) = total {
        // Mark sparse *before* extending, so NTFS never zero-fills the whole range.
        //
        // Note we deliberately do not use SetFileValidData, which would be faster still: it
        // requires SeManageVolumePrivilege and can expose previously-deleted disk contents
        // inside the user's file. That is a real security trade-off, and the speed does not
        // justify it.
        set_sparse(&file);
        if file.metadata()?.len() < total {
            file.set_len(total)?;
        }
    }
    Ok(file)
}

#[cfg(windows)]
fn set_sparse(file: &File) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    const FSCTL_SET_SPARSE: u32 = 0x000900C4;
    let mut returned = 0u32;
    unsafe {
        DeviceIoControl(
            file.as_raw_handle() as _,
            FSCTL_SET_SPARSE,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        );
    }
}

#[cfg(not(windows))]
fn set_sparse(_file: &File) {
    // Files are sparse by default on Linux; nothing to request.
}

/// Positional write. No shared file cursor, so the single writer task can place bytes for any
/// segment without seeking state.
#[cfg(windows)]
pub fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut written = 0usize;
    while written < buf.len() {
        let n = file.seek_write(&buf[written..], offset + written as u64)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "seek_write wrote nothing"));
        }
        written += n;
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buf, offset)
}

/// Positional read, used to compare already-downloaded bytes against a replacement URL.
#[cfg(windows)]
pub fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    let mut read = 0usize;
    while read < buf.len() {
        match file.seek_read(&mut buf[read..], offset + read as u64)? {
            0 => break,
            n => read += n,
        }
    }
    Ok(read)
}

#[cfg(not(windows))]
pub fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    let mut read = 0usize;
    while read < buf.len() {
        match file.read_at(&mut buf[read..], offset + read as u64)? {
            0 => break,
            n => read += n,
        }
    }
    Ok(read)
}

/// Free space on the volume holding `path`.
#[cfg(windows)]
pub fn free_space(path: &Path) -> io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let mut avail = 0u64;
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, std::ptr::null_mut(), std::ptr::null_mut()) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(avail)
}

#[cfg(unix)]
pub fn free_space(path: &Path) -> io::Result<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st.f_bavail as u64 * st.f_frsize as u64)
}

/// Check there is room for `needed` bytes plus a margin, before starting.
pub fn ensure_space(dir: &Path, needed: u64) -> io::Result<()> {
    // Walk up to the nearest existing ancestor: the destination may not exist yet.
    let mut probe = dir;
    loop {
        if probe.exists() {
            break;
        }
        match probe.parent() {
            Some(p) => probe = p,
            None => return Ok(()), // nothing to check against
        }
    }
    let avail = free_space(probe)?;
    if avail < needed.saturating_add(FREE_SPACE_MARGIN) {
        return Err(io::Error::new(
            io::ErrorKind::StorageFull,
            format!(
                "need {} plus a {} margin but only {} is free",
                human(needed),
                human(FREE_SPACE_MARGIN),
                human(avail)
            ),
        ));
    }
    Ok(())
}

/// Claim the final name. Only ever called once every byte is present and durable.
pub fn finalize(part: &Path, final_path: &Path, overwrite: bool) -> io::Result<()> {
    if final_path.exists() {
        if !overwrite {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} already exists", final_path.display()),
            ));
        }
        std::fs::remove_file(final_path)?;
    }
    std::fs::rename(part, final_path)
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_path_is_distinct_from_the_final_name() {
        let p = part_path(Path::new("/downloads"), "movie.mkv");
        assert_eq!(p, PathBuf::from("/downloads/movie.mkv.slpart"));
        assert_ne!(p.file_name(), Some(std::ffi::OsStr::new("movie.mkv")));
    }

    #[test]
    fn preallocates_to_the_requested_size() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.slpart");
        let f = open_part(&p, Some(1_048_576)).unwrap();
        assert_eq!(f.metadata().unwrap().len(), 1_048_576);
    }

    #[test]
    fn reopening_never_shrinks_existing_data() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.slpart");
        {
            let f = open_part(&p, Some(4096)).unwrap();
            write_at(&f, b"hello", 0).unwrap();
            f.sync_all().unwrap();
        }
        let f = open_part(&p, Some(4096)).unwrap();
        let mut buf = [0u8; 5];
        read_at(&f, &mut buf, 0).unwrap();
        assert_eq!(&buf, b"hello", "resume must not truncate what is already there");
    }

    #[test]
    fn positional_writes_land_out_of_order_correctly() {
        // Exactly what the writer task does: segments finish in any order.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.slpart");
        let f = open_part(&p, Some(12)).unwrap();

        write_at(&f, b"world", 6).unwrap();
        write_at(&f, b"hello ", 0).unwrap();
        write_at(&f, b"!", 11).unwrap();
        f.sync_all().unwrap();

        let mut buf = [0u8; 12];
        assert_eq!(read_at(&f, &mut buf, 0).unwrap(), 12);
        assert_eq!(&buf, b"hello world!");
    }

    #[test]
    fn read_at_reports_a_short_read_at_eof() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.slpart");
        let f = open_part(&p, Some(10)).unwrap();
        let mut buf = [0u8; 100];
        assert_eq!(read_at(&f, &mut buf, 0).unwrap(), 10);
        assert_eq!(read_at(&f, &mut buf, 10).unwrap(), 0);
    }

    #[test]
    fn free_space_is_reported_for_an_existing_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(free_space(dir.path()).unwrap() > 0);
    }

    #[test]
    fn ensure_space_rejects_an_impossible_request() {
        let dir = tempfile::tempdir().unwrap();
        assert!(ensure_space(dir.path(), 1024).is_ok());

        let err = ensure_space(dir.path(), u64::MAX / 2).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::StorageFull);
        assert!(err.to_string().contains("free"), "{err}");
    }

    #[test]
    fn ensure_space_walks_up_to_an_existing_ancestor() {
        // The destination directory may not have been created yet.
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b/c");
        assert!(ensure_space(&nested, 1024).is_ok());
    }

    #[test]
    fn finalize_refuses_to_clobber_without_permission() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("f.slpart");
        let dest = dir.path().join("f.bin");
        std::fs::write(&part, b"new").unwrap();
        std::fs::write(&dest, b"existing").unwrap();

        let err = finalize(&part, &dest, false).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&dest).unwrap(), b"existing", "must not have been touched");

        finalize(&part, &dest, true).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        assert!(!part.exists(), "part file should be gone after finalize");
    }

    #[test]
    fn human_sizes_read_sensibly() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1536), "1.5 KB");
        assert_eq!(human(10 * 1024 * 1024 * 1024), "10.0 GB");
    }
}

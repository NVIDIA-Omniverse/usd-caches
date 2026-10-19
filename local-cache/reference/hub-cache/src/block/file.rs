// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use parking_lot::RwLock;
use tracing::{debug, info, instrument, trace, warn};
use walkdir::WalkDir;

use hub_util::fs::{create_dir, remove_dir_all, remove_file};

use super::record;
use super::{AtomicCell, Error, Result};

// on purpose using std::fs here instead of tokio::fs
// tokio::fs is heavy weight in its async implementation (just wraps std::fs calls with spawn_blocking())
// and we currently do not really benefit from the async fs

#[derive(Debug)]
pub(super) struct DataLocation {
    id: u64,
    offset: u64,
}

impl DataLocation {
    fn new(id: u64, offset: u64) -> Self {
        Self { id, offset }
    }

    pub(super) fn id(&self) -> u64 {
        self.id
    }

    pub(super) fn offset(&self) -> u64 {
        self.offset
    }
}

#[derive(Debug, PartialEq)]
enum FileKind {
    Block,
    File,
}

struct File {
    kind: FileKind,
    // file id
    id: u64,
    // file path
    path: PathBuf,
    // file size so we do not have to stat() each time we attempt get data location
    size: AtomicCell<u64>,
    // current offset
    offset: AtomicU64,
    // file handle
    file: fs::File,
}

// annoyingly you cannot exclude a field from the debug macro
// https://github.com/rust-lang/rust/issues/37009
impl fmt::Debug for File {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "File {{ kind: {:?}, id: {}, offset: {}, size: {} path: {:?} }}",
            self.kind,
            self.id,
            self.offset.load(Ordering::Relaxed),
            self.size.get(),
            self.path,
        )
    }
}

impl File {
    fn prefix_dir(id: u64) -> PathBuf {
        PathBuf::from(format!("{:03X}", id & 0xFFF))
    }

    fn file_name(id: u64, extension: &Option<Arc<str>>) -> PathBuf {
        let mut path = PathBuf::from(format!("{id:016X}"));

        if let Some(extension) = extension {
            path.set_extension(extension.as_ref());
        }

        path
    }

    /// create new file with `id` at `base_path` with a file size of `size` and optional file `extension`.
    #[instrument(level = "debug", skip(base_path), err)]
    fn create<P>(id: u64, base_path: P, size: u64, extension: &Option<Arc<str>>) -> Result<Self>
    where
        P: AsRef<Path> + std::fmt::Debug,
    {
        let prefix_dir = base_path.as_ref().join(Self::prefix_dir(id));
        create_dir(&prefix_dir)?;

        let path = prefix_dir.join(Self::file_name(id, extension));
        info!(?path);

        // open file, must be created new
        let file = fs::OpenOptions::new()
            .create_new(true)
            .read(false)
            .write(true)
            .open(&path)?;

        let kind = match extension.is_none() {
            true => {
                // sparsely pre-allocate file if block kind
                Self::expand(&file, size)?;
                FileKind::Block
            },
            false => FileKind::File,
        };

        Ok(Self {
            kind,
            id,
            path,
            size: AtomicCell::new(size),
            offset: AtomicU64::new(0),
            file,
        })
    }

    /// get file id
    fn id(&self) -> u64 {
        self.id
    }

    /// get file size
    fn size(&self) -> u64 {
        self.size.get()
    }

    /// get offset for writing `size` bytes. returns `Error::BlockFileOffsetOverflow` if `size` cannot fit
    /// in order to minimize fragmentation and unused holes at the end of each file, we allow
    /// the underlying file size to actually vary from `self.size` to `self.size * 2`
    #[instrument(level = "debug", skip(self), err, fields(extra))]
    fn next_location(&self, size: u64) -> Result<DataLocation> {
        // grab the next available offset
        let offset = self.offset.fetch_add(size, Ordering::Relaxed);

        match self.kind {
            FileKind::Block => {
                // blocks must be > 0 in size
                if size == 0 {
                    return Err(Error::InvalidArgument("size must be > 0".to_owned()));
                }

                // if the offset is >= size, overflow happened and this file is no longer usable
                if offset >= self.size() {
                    return Err(Error::BlockFileOffsetOverflow(self.id));
                }

                // check to see if we overflowed
                let extra = offset + size;
                record!(extra);

                // if extra > self.size, we need to extend the file (kind == block) to fit this overflow
                if extra > self.size() {
                    Self::expand(&self.file, extra)?;
                }
            },
            FileKind::File => {
                // file kind should be unique, always offset 0
                assert_eq!(offset, 0);
            },
        }

        Ok(DataLocation::new(self.id, offset))
    }

    /// sparsely expand backing file
    #[cfg(target_family = "unix")]
    #[instrument(level = "debug", skip(file), err)]
    fn expand(file: &fs::File, size: u64) -> Result<()> {
        use std::os::fd::{AsFd, AsRawFd};

        let fd = file.as_fd().as_raw_fd();
        let ret = unsafe { libc::ftruncate(fd, size as i64) };

        if ret == -1 {
            Err(std::io::Error::last_os_error().into())
        } else {
            Ok(())
        }
    }

    /// sparsely expand backing file
    #[cfg(target_family = "windows")]
    #[instrument(level = "debug", skip(file), err)]
    fn expand(file: &fs::File, size: u64) -> Result<()> {
        use std::os::windows::prelude::AsRawHandle;
        use windows::Win32::{
            Foundation::HANDLE,
            Storage::FileSystem::{SetEndOfFile, SetFilePointerEx, FILE_BEGIN, FILE_CURRENT},
        };

        let handle = HANDLE(file.as_raw_handle() as isize);

        unsafe {
            SetFilePointerEx(handle, size as i64, None, FILE_CURRENT)?;
            SetEndOfFile(handle)?;
            SetFilePointerEx(handle, 0, None, FILE_BEGIN)?;
        }
        Ok(())
    }
}

/// Manages the data files of the cache.
///
/// TODO: `size_on_disk` is currently always 0. there is some difficultly in keeping this stat
///        accurate, since file kind files are allocated with size == 0, only later after the
///        file is closed, is the file on disk stat()'d and its true/final size known
#[derive(Debug)]
pub(super) struct FilePool {
    // path where all the files are stored
    base_path: PathBuf,
    // size of block files created in this pool in bytes
    file_size: AtomicCell<u64>,
    // tracks the next file id to give out
    next_file_id: AtomicU64,
    // current file to try and allocate space from
    current_file: RwLock<File>,
    // total size of files on disk in bytes
    size_on_disk: AtomicU64,
}

pub(super) type FileMap = BTreeMap<u64, (u64, PathBuf)>;
pub(super) type UnknownFileSet = BTreeSet<PathBuf>;

impl FilePool {
    #[instrument(level = "debug", ret)]
    pub(super) fn new<P>(base_path: P, file_size: u64) -> Result<Self>
    where
        P: AsRef<Path> + std::fmt::Debug,
    {
        // base path should already exist!
        let base_path = fs::canonicalize(base_path.as_ref())?;
        assert!(base_path.exists());

        // pending files -- remove this directory if it exists, no longer used
        let pending_path = base_path.join("hub-pending");
        remove_dir_all(pending_path)?;

        let base_path = base_path.join("hub-files");
        create_dir(&base_path)?;

        let mut next_file_id = None;

        // TODO: does file id wrapping ever matter? 2^64 files?

        // scan all the files and store into our map
        for entry in Self::walk_dir(&base_path)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            match entry
                .path()
                .file_stem()
                .map(|stem| u64::from_str_radix(&stem.to_string_lossy(), 16))
            {
                Some(Ok(id)) => {
                    debug!(id, path = ?entry.path(), "found file");
                    // look for the highest file id
                    next_file_id = std::cmp::max(next_file_id, Some(id));
                },
                _ => {
                    warn!(path = ?entry.path(), "unknown file found in cache directory!");
                    // TODO: remove this unknown file?
                },
            }
        }

        // start at zero or one more than the highest file no found
        let next_file_id = next_file_id.map_or(0, |id| id + 1);

        let current_file = File::create(next_file_id, &base_path, file_size, &None)?;

        Ok(Self {
            base_path,
            file_size: AtomicCell::new(file_size),
            next_file_id: AtomicU64::new(next_file_id + 1),
            current_file: RwLock::new(current_file),
            size_on_disk: AtomicU64::new(0),
        })
    }

    // get base path for all files
    pub(super) fn base_path(&self) -> &Path {
        self.base_path.as_path()
    }

    // get size of all the files on disk
    pub(super) fn size_on_disk(&self) -> u64 {
        self.size_on_disk.load(Ordering::Relaxed)
    }

    // file path, relative to base_path for a given id and optional extension
    pub(super) fn file_path(id: u64, extension: &Option<Arc<str>>) -> PathBuf {
        File::prefix_dir(id).join(File::file_name(id, extension))
    }

    // find the available data location for a given `size`
    // if `size` > `file_size` or `extension` is set, allocate a separate file
    // if `current_file` overflows, allocate new file and retry allocation
    #[instrument(level = "debug", skip(self), ret)]
    pub(super) fn next_data_location(&self, size: u64, extension: &Option<Arc<str>>) -> Result<DataLocation> {
        loop {
            let location_result = {
                let current_file = self.current_file.read();

                if size > current_file.size() || extension.is_some() {
                    // if the size is larger than the data file size or unique file was requested (by presense of extension),
                    // just make a separate file for this entry, we do not need to block current file any longer
                    drop(current_file);

                    let file = self.get_next_file(size, extension)?;
                    file.next_location(size)
                } else {
                    current_file.next_location(size)
                }
            };

            match location_result {
                Ok(location) => break Ok(location),
                Err(Error::BlockFileOffsetOverflow(id)) => {
                    // we only need one to grab the write lock and successfully swap a new file in
                    if let Some(mut current_file) = self.current_file.try_write() {
                        // if file changed since we acquired lock, loop back around and retest
                        if current_file.id() != id {
                            continue;
                        }

                        // create new file and
                        let file = self.get_next_file(self.file_size.get(), &None)?;
                        *current_file = file;
                    }
                    // loop around, if self.file_size changed, we should retest size
                    continue;
                },
                Err(e) => break Err(e),
            }
        }
    }

    // get next active file
    #[instrument(level = "debug", skip(self), ret, fields(id))]
    fn get_next_file(&self, size: u64, extension: &Option<Arc<str>>) -> Result<File> {
        let id = self.next_file_id.fetch_add(1, Ordering::Relaxed);
        record!(id);

        File::create(id, &self.base_path, size, extension)
    }

    #[instrument(level = "debug", skip(self), ret)]
    pub(super) fn remove_file<P>(&self, id: u64, size: u64, path: P) -> Result<bool>
    where
        P: AsRef<Path> + std::fmt::Debug,
    {
        let current_file = self.current_file.read();

        if current_file.id() != id {
            Ok(remove_file(path)?)
        } else {
            // not an error, but we don't want to remove the current active file, even if its empty (offset == 0)
            Ok(false)
        }
    }

    // get all the files in the active path -> data files, unknown files
    // we return one map of the active files [id] -> PathBuf
    // and one set of files found not matching our hex filename format (unknown)
    #[instrument(level = "debug", skip(self), err)]
    pub(super) fn files(&self) -> Result<(FileMap, UnknownFileSet)> {
        let mut data_files = BTreeMap::new();
        let mut unknown_files = BTreeSet::new();

        for entry in Self::walk_dir(self.base_path())
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            trace!(path = ?entry.path());

            match entry
                .path()
                .file_stem()
                .map(|stem| u64::from_str_radix(&stem.to_string_lossy(), 16))
            {
                Some(Ok(id)) => {
                    let inserted = data_files.insert(id, (entry.metadata()?.len(), entry.into_path()));
                    assert!(inserted.is_none());
                },
                _ => {
                    let inserted = unknown_files.insert(entry.into_path());
                    assert!(inserted);
                },
            }
        }

        Ok((data_files, unknown_files))
    }

    // walk directory common options
    fn walk_dir(path: impl AsRef<Path>) -> WalkDir {
        WalkDir::new(path).same_file_system(true).follow_links(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use rstest::rstest;
    use tempfile::tempdir;
    use walkdir::WalkDir;

    #[rstest]
    #[case(1)]
    #[case(2)]
    #[case(4)]
    #[case(420)]
    #[case(1000)]
    #[case(2048)]
    #[case(5001)]
    #[test_log::test]
    fn create(#[case] size: u64) {
        let tmp_dir = tempdir().unwrap();
        let path = tmp_dir.path();
        let id = 789;
        let file = File::create(id, path, size, &None).unwrap();
        assert_eq!(file.id(), id);
        assert_eq!(file.path, path.join(FilePool::file_path(id, &None)));

        // invalid size
        assert_matches!(file.next_location(0).unwrap_err(), Error::InvalidArgument(_));

        // size of 1
        let location = file.next_location(1).unwrap();
        assert_eq!(location.id, file.id());
        assert_eq!(location.offset, 0);

        // size of remaining
        let remaining = size - 1;

        // case 1, there is no remaining bytes
        if remaining > 0 {
            let location = file.next_location(remaining).unwrap();
            assert_eq!(location.offset, 1);
        }

        // size overflow
        let err = file.next_location(1).unwrap_err();
        assert_matches!(err, Error::BlockFileOffsetOverflow(_));

        // overflow again
        let err = file.next_location(999).unwrap_err();
        assert_matches!(err, Error::BlockFileOffsetOverflow(_));
    }

    #[test_log::test]
    fn overflow() {
        let tmp_dir = tempdir().unwrap();

        {
            let path = tmp_dir.path();
            let file = File::create(0, path, 1024, &None).unwrap();
            assert_eq!(file.size.get(), 1024);
            assert_eq!(file.path.metadata().unwrap().len(), 1024);

            // size - 1
            let location = file.next_location(1023).unwrap();
            assert_eq!(location.id, file.id());
            assert_eq!(location.offset, 0);
            assert_eq!(file.size.get(), 1024);
            assert_eq!(file.path.metadata().unwrap().len(), 1024);

            // up to size
            let location = file.next_location(1).unwrap();
            assert_eq!(location.id, file.id());
            assert_eq!(location.offset, 1023);
            assert_eq!(file.size.get(), 1024);
            assert_eq!(file.path.metadata().unwrap().len(), 1024);

            // overflow
            let err = file.next_location(1).unwrap_err();
            assert_matches!(err, Error::BlockFileOffsetOverflow(_));
            assert_eq!(file.size.get(), 1024);
            assert_eq!(file.path.metadata().unwrap().len(), 1024);
        }

        {
            let path = tmp_dir.path();
            let file = File::create(1, path, 1024, &None).unwrap();
            assert_eq!(file.size.get(), 1024);
            assert_eq!(file.path.metadata().unwrap().len(), 1024);

            // overflow on first write
            let location = file.next_location(2048).unwrap();
            assert_eq!(location.id, file.id());
            assert_eq!(location.offset, 0);
            // cached file size does not change
            assert_eq!(file.size.get(), 1024);
            // on disk file should have been expanded
            assert_eq!(file.path.metadata().unwrap().len(), 2048);

            // overflow
            let err = file.next_location(1).unwrap_err();
            assert_matches!(err, Error::BlockFileOffsetOverflow(_));
        }

        {
            let path = tmp_dir.path();
            let file = File::create(2, path, 1024, &None).unwrap();
            assert_eq!(file.size.get(), 1024);
            assert_eq!(file.path.metadata().unwrap().len(), 1024);

            // size / 2
            let location = file.next_location(512).unwrap();
            assert_eq!(location.id, file.id());
            assert_eq!(location.offset, 0);
            assert_eq!(file.size.get(), 1024);
            assert_eq!(file.path.metadata().unwrap().len(), 1024);

            // allow overflow
            let location = file.next_location(1530).unwrap();
            assert_eq!(location.id, file.id());
            assert_eq!(location.offset, 512);
            assert_eq!(file.size.get(), 1024);
            assert_eq!(file.path.metadata().unwrap().len(), 2042);

            // overflow
            let err = file.next_location(1).unwrap_err();
            assert_matches!(err, Error::BlockFileOffsetOverflow(_));
            assert_eq!(file.size.get(), 1024);
            assert_eq!(file.path.metadata().unwrap().len(), 2042);
        }
    }

    #[test_log::test]
    fn invalid_args() {
        let tmp_dir = tempdir().unwrap();

        // invalid path
        let result = File::create(2, Path::new("daija/aera3/a2344/testfile"), 420, &None);
        assert_matches!(result.unwrap_err(), Error::Io(_));

        // base path is a file
        let _file = fs::OpenOptions::new()
            .create_new(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(tmp_dir.path().join("foo"))
            .unwrap();
        let result = File::create(3, tmp_dir.path().join("foo"), 666, &None);
        assert_matches!(result.unwrap_err(), Error::Io(_));
    }

    #[test_log::test]
    fn already_exists() {
        let tmp_dir = tempdir().unwrap();
        let path = tmp_dir.path();
        let _file = File::create(0, path, 128, &None).unwrap();

        // file exists
        let result = File::create(0, path, 128, &None);
        assert_matches!(result.unwrap_err(), Error::Io(_));
    }

    #[rstest]
    #[case(4000, 1000)]
    #[case(4096, 1024)]
    #[test_log::test]
    fn next_block(#[case] file_size: u64, #[case] increment: u64) {
        let tmp_dir = tempdir().unwrap();
        let pool = FilePool::new(tmp_dir.path(), file_size).unwrap();
        for offset in (0..increment * 1000).step_by(increment as usize) {
            let location = pool.next_data_location(increment, &None).unwrap();
            assert_eq!(location.id(), offset / file_size);
            assert_eq!(location.offset(), offset % file_size);
        }
    }

    // Counts the number of files in a directory
    fn count_files_in_dir(path: impl AsRef<Path>) -> usize {
        WalkDir::new(path.as_ref())
            .into_iter()
            .filter(|entry| entry.as_ref().unwrap().file_type().is_file())
            .count()
    }

    #[test_log::test]
    fn unique_file() {
        let tmp_dir = tempdir().unwrap();
        let cache_path = tmp_dir.path();

        let pool = FilePool::new(cache_path, 4096).unwrap();
        assert_eq!(1, count_files_in_dir(cache_path));

        // location for unique file
        let extension: Option<Arc<str>> = Some(Arc::from("txt"));
        let location = pool.next_data_location(666, &extension).unwrap();
        assert_eq!(2, count_files_in_dir(cache_path));
        assert_eq!(location.id, 1);
        assert_eq!(location.offset, 0);

        // since this is a file with extension, file won't be expanded, size == 0
        let path = pool.base_path().join(FilePool::file_path(location.id, &extension));
        assert_eq!(path.metadata().unwrap().len(), 0);

        let path = pool.base_path().join(FilePool::file_path(0, &None));
        assert_eq!(path.metadata().unwrap().len(), 4096);

        // since we added a unique file, the initial block file, id == 0, should still be at offset == 0
        assert_eq!(pool.current_file.read().offset.load(Ordering::Relaxed), 0);

        // separate file for large size
        let location = pool.next_data_location(8192, &None).unwrap();
        assert_eq!(3, count_files_in_dir(cache_path));
        assert_eq!(location.id, 2);
        assert_eq!(location.offset, 0);
        let path = pool.base_path().join(FilePool::file_path(location.id(), &None));
        assert_eq!(path.metadata().unwrap().len(), 8192);

        // cause overflow
        let location = pool.next_data_location(4000, &None).unwrap();
        assert_eq!(location.id, 0);
        assert_eq!(location.offset, 0);
        assert_eq!(3, count_files_in_dir(cache_path));
        let location = pool.next_data_location(1000, &None).unwrap();
        assert_eq!(location.id, 0);
        assert_eq!(location.offset, 4000);
        assert_eq!(3, count_files_in_dir(cache_path));
        let path = pool.base_path().join(FilePool::file_path(location.id(), &None));
        // file would have been expanded
        assert_eq!(path.metadata().unwrap().len(), 5000);

        // overflow will happen now
        let location = pool.next_data_location(1000, &None).unwrap();
        assert_eq!(location.id, 3);
        assert_eq!(location.offset, 0);
        assert_eq!(4, count_files_in_dir(cache_path));
        let path = pool.base_path().join(FilePool::file_path(location.id, &None));
        assert_eq!(path.metadata().unwrap().len(), 4096);

        let location = pool.next_data_location(300, &None).unwrap();
        assert_eq!(location.id, 3);
        assert_eq!(location.offset, 1000);
        assert_eq!(4, count_files_in_dir(cache_path));
        assert_eq!(path.metadata().unwrap().len(), 4096);
    }

    #[test_log::test]
    fn file_path() {
        let tmp_dir = tempdir().unwrap();
        let cache_path = tmp_dir.path();

        let pool = FilePool::new(cache_path, 4096).unwrap();
        assert_eq!(1, count_files_in_dir(cache_path));

        let path = pool.base_path().join(FilePool::file_path(0, &None));
        assert_eq!(path.metadata().unwrap().len(), 4096);

        let location = pool.next_data_location(3000, &None).unwrap();
        assert_eq!(location.id(), 0);
        assert_eq!(location.offset(), 0);

        // still the same size
        let path = pool.base_path().join(FilePool::file_path(0, &None));
        assert_eq!(path.metadata().unwrap().len(), 4096);

        let location = pool.next_data_location(2000, &None).unwrap();
        assert_eq!(location.id(), 0);
        assert_eq!(location.offset(), 3000);

        // should have expanded 4096 -> 5000
        let path = pool.base_path().join(FilePool::file_path(0, &None));
        assert_eq!(path.metadata().unwrap().len(), 5000);

        // overflow and rotate to new file
        let location = pool.next_data_location(1000, &None).unwrap();
        assert_eq!(location.id(), 1);
        assert_eq!(location.offset(), 0);

        // test id == 0
        let path = pool.base_path().join(FilePool::file_path(0, &None));
        assert_eq!(path.metadata().unwrap().len(), 5000);

        // test id == 1
        let path = pool.base_path().join(FilePool::file_path(1, &None));
        assert_eq!(path.metadata().unwrap().len(), 4096);

        // file kind
        let location = pool.next_data_location(222, &Some(Arc::from("png"))).unwrap();
        assert_eq!(location.id(), 2);
        assert_eq!(location.offset(), 0);

        // test id == 2, size should be zero for file kind
        let path = pool.base_path().join(FilePool::file_path(2, &Some(Arc::from("png"))));
        assert_eq!(path.metadata().unwrap().len(), 0);

        // test ids that does not exist
        let path = pool.base_path().join(FilePool::file_path(10, &None));
        assert!(!path.exists());
        let path = pool
            .base_path()
            .join(FilePool::file_path(9984141, &Some(Arc::from("JPG"))));
        assert!(!path.exists());
    }

    // sums the total bytes of all files in a directory (recursively)
    fn size_of_files_in_dir(path: impl AsRef<Path>) -> u64 {
        WalkDir::new(path.as_ref())
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .map(|e| e.metadata().unwrap().len())
            .sum()
    }

    #[rstest]
    #[case(0)]
    #[case(1)]
    #[case(2)]
    #[case(4)]
    #[case(10)]
    #[case(42)]
    #[test_log::test]
    fn files(#[case] number_of_files: u64) {
        let tmp_dir = tempdir().unwrap();
        let cache_path = tmp_dir.path();

        let pool = FilePool::new(cache_path, 32).unwrap();

        // keep adding files by overflowing the pool
        // test that the correct number of data_files is detected

        for id in 0..number_of_files {
            let location = pool.next_data_location(32, &None).unwrap();
            assert_eq!(location.id, id);

            let (data_files, unknown_files) = pool.files().unwrap();
            assert_eq!(id + 1, data_files.len() as u64);
            assert_eq!(0, unknown_files.len());
        }

        let (data_files, unknown_files) = pool.files().unwrap();

        let data_files_bytes: u64 = data_files.iter().map(|f| f.1 .0).sum();
        assert_eq!(data_files_bytes, size_of_files_in_dir(cache_path));

        if number_of_files == 0 {
            assert_eq!(1, data_files.len());
            assert_eq!(0, unknown_files.len());
        } else {
            assert_eq!(number_of_files, data_files.len() as u64);
            assert_eq!(0, unknown_files.len());
        }
    }
}

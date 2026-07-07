use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request,
};
use libc::{
    EACCES, EBADF, EEXIST, EINVAL, EIO, EISDIR, ENOENT, ENOSYS, ENOTDIR, O_ACCMODE, O_APPEND,
    O_RDONLY, O_RDWR, O_TRUNC, O_WRONLY,
};
use tracing::{debug, error};

use crate::clock::now_unix_seconds;
use crate::duration::Ttl;
use crate::layout::{FADE_DIR, init_backing_dir, metadata_db_path};
use crate::metadata::{FileRecord, FileState, MetadataStore};
use crate::path::{RelativePath, safe_join};
use crate::policy::DevPolicy;
use crate::{FadeError, Result};

const ROOT_INO: u64 = 1;
const ATTR_TTL: Duration = Duration::from_secs(1);
const DEFAULT_TTL_FOLDERS: &[&str] = &["1m", "1h", "24h", "7d", "30d", "forever"];

pub fn mount_dev(
    backing_dir: &Path,
    mountpoint: &Path,
    recovery_window: Duration,
    reaper_interval: Duration,
) -> anyhow::Result<()> {
    init_backing_dir(backing_dir)?;
    ensure_default_ttl_folders(backing_dir)?;

    let db_path = metadata_db_path(backing_dir);
    let store = MetadataStore::open(&db_path)?;
    store.expire_due(now_unix_seconds())?;

    let reaper = ReaperThread::spawn(backing_dir.to_path_buf(), db_path, reaper_interval);
    let filesystem = DevFuse::new(backing_dir.to_path_buf(), store, recovery_window);
    let options = [
        MountOption::FSName("fade".to_string()),
        MountOption::Subtype("fade".to_string()),
        MountOption::AutoUnmount,
        MountOption::DefaultPermissions,
        MountOption::NoDev,
        MountOption::NoSuid,
    ];

    let result = fuser::mount2(filesystem, mountpoint, &options)
        .with_context(|| format!("unable to mount `{}`", mountpoint.display()));

    reaper.stop();
    result
}

fn ensure_default_ttl_folders(backing_dir: &Path) -> Result<()> {
    for folder in DEFAULT_TTL_FOLDERS {
        fs::create_dir_all(backing_dir.join(folder))?;
    }
    Ok(())
}

struct ReaperThread {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ReaperThread {
    fn spawn(backing_dir: PathBuf, db_path: PathBuf, interval: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let interval = interval.max(Duration::from_secs(1));

        let handle = thread::spawn(move || {
            let mut next_run = Instant::now() + interval;
            while !thread_stop.load(Ordering::Relaxed) {
                if Instant::now() >= next_run {
                    run_reaper_pass(&backing_dir, &db_path);
                    next_run = Instant::now() + interval;
                }
                thread::sleep(Duration::from_millis(250));
            }
        });

        Self {
            stop,
            handle: Some(handle),
        }
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run_reaper_pass(backing_dir: &Path, db_path: &Path) {
    let now = now_unix_seconds();
    let started = Instant::now();
    let store = match MetadataStore::open(db_path) {
        Ok(store) => store,
        Err(error) => {
            error!(%error, "failed to open metadata for reaper pass");
            return;
        }
    };

    if let Err(error) = crate::reaper::run_once(&store, backing_dir, now) {
        let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let message = error.to_string();
        let _ = store.record_reaper_run(now, duration_ms, Some(&message));
        error!(%error, "reaper pass failed");
    }
}

struct OpenHandle {
    file: File,
    path: RelativePath,
}

struct DevFuse {
    backing_dir: PathBuf,
    store: MetadataStore,
    policy: DevPolicy,
    recovery_window: Duration,
    next_ino: u64,
    next_fh: u64,
    paths_by_ino: HashMap<u64, RelativePath>,
    inos_by_path: HashMap<String, u64>,
    open_handles: HashMap<u64, OpenHandle>,
}

impl DevFuse {
    fn new(backing_dir: PathBuf, store: MetadataStore, recovery_window: Duration) -> Self {
        let mut paths_by_ino = HashMap::new();
        let mut inos_by_path = HashMap::new();
        paths_by_ino.insert(ROOT_INO, RelativePath::root());
        inos_by_path.insert(String::new(), ROOT_INO);

        Self {
            backing_dir,
            store,
            policy: DevPolicy,
            recovery_window,
            next_ino: ROOT_INO + 1,
            next_fh: 1,
            paths_by_ino,
            inos_by_path,
            open_handles: HashMap::new(),
        }
    }

    fn path_for_ino(&self, ino: u64) -> std::result::Result<RelativePath, i32> {
        self.paths_by_ino.get(&ino).cloned().ok_or(ENOENT)
    }

    fn inode_for_path(&mut self, path: &RelativePath) -> u64 {
        if let Some(ino) = self.inos_by_path.get(path.as_str()) {
            return *ino;
        }

        let ino = self.next_ino;
        self.next_ino += 1;
        self.paths_by_ino.insert(ino, path.clone());
        self.inos_by_path.insert(path.as_str().to_string(), ino);
        ino
    }

    fn remove_inode_for_path(&mut self, path: &RelativePath) {
        if let Some(ino) = self.inos_by_path.remove(path.as_str()) {
            self.paths_by_ino.remove(&ino);
        }
    }

    fn rename_inode_paths(&mut self, from: &RelativePath, to: &RelativePath) {
        let from_prefix = format!("{}/", from.as_str());
        let updates: Vec<_> = self
            .paths_by_ino
            .iter()
            .filter_map(|(ino, path)| {
                if path == from {
                    return Some((*ino, to.clone()));
                }

                path.as_str().strip_prefix(&from_prefix).and_then(|suffix| {
                    RelativePath::new(format!("{}/{}", to.as_str(), suffix))
                        .ok()
                        .map(|path| (*ino, path))
                })
            })
            .collect();

        for (ino, new_path) in updates {
            if let Some(old_path) = self.paths_by_ino.insert(ino, new_path.clone()) {
                self.inos_by_path.remove(old_path.as_str());
            }
            self.inos_by_path.insert(new_path.as_str().to_string(), ino);
        }
    }

    fn child_path(&self, parent: u64, name: &OsStr) -> std::result::Result<RelativePath, i32> {
        let parent_path = self.path_for_ino(parent)?;
        if name == FADE_DIR {
            return Err(ENOENT);
        }

        let Some(name) = name.to_str() else {
            return Err(EINVAL);
        };
        parent_path.join_segment(name).map_err(errno_for_fade)
    }

    fn backing_path(&self, path: &RelativePath) -> std::result::Result<PathBuf, i32> {
        safe_join(&self.backing_dir, path).map_err(errno_for_fade)
    }

    fn visible_attr(&mut self, path: &RelativePath) -> std::result::Result<FileAttr, i32> {
        let backing_path = self.backing_path(path)?;
        let metadata = fs::symlink_metadata(&backing_path).map_err(errno_for_io)?;
        let file_type = metadata.file_type();

        if file_type.is_symlink() {
            return Err(ENOENT);
        }

        if file_type.is_dir() {
            if !self.directory_visible(path) {
                return Err(ENOENT);
            }
        } else if file_type.is_file() {
            self.visible_file_record(path)?;
        } else {
            return Err(ENOENT);
        }

        let ino = self.inode_for_path(path);
        Ok(file_attr(ino, &metadata))
    }

    fn visible_file_record(&mut self, path: &RelativePath) -> std::result::Result<FileRecord, i32> {
        self.store
            .expire_due(now_unix_seconds())
            .map_err(errno_for_fade)?;
        let record = self
            .store
            .get_by_path(path)
            .map_err(errno_for_fade)?
            .ok_or(ENOENT)?;

        match record.state {
            FileState::Alive | FileState::Recovered => Ok(record),
            FileState::Expired | FileState::Deleted => Err(ENOENT),
        }
    }

    fn directory_visible(&self, path: &RelativePath) -> bool {
        if path.is_root() {
            return true;
        }

        path.components()
            .next()
            .is_some_and(|first| Ttl::parse(first).is_ok())
    }

    fn parent_directory_visible(&self, path: &RelativePath) -> bool {
        path.parent()
            .as_ref()
            .is_some_and(|parent| self.directory_visible(parent))
    }

    fn open_handle(&mut self, file: File, path: RelativePath) -> u64 {
        let fh = self.next_fh;
        self.next_fh += 1;
        self.open_handles.insert(fh, OpenHandle { file, path });
        fh
    }

    fn refresh_observed_size(&self, path: &RelativePath) -> std::result::Result<(), i32> {
        let backing_path = self.backing_path(path)?;
        let metadata = fs::metadata(backing_path).map_err(errno_for_io)?;
        self.store
            .update_observed_size(path, metadata.len(), now_unix_seconds())
            .map_err(errno_for_fade)
    }
}

impl Filesystem for DevFuse {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        match self
            .child_path(parent, name)
            .and_then(|path| self.visible_attr(&path))
        {
            Ok(attr) => reply.entry(&ATTR_TTL, &attr, 0),
            Err(errno) => reply.error(errno),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        match self
            .path_for_ino(ino)
            .and_then(|path| self.visible_attr(&path))
        {
            Ok(attr) => reply.attr(&ATTR_TTL, &attr),
            Err(errno) => reply.error(errno),
        }
    }

    fn mkdir(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let result = (|| {
            let path = self.child_path(parent, name)?;
            if path.parent().is_some_and(|parent| parent.is_root()) {
                self.policy
                    .validate_root_policy_folder(path.file_name().unwrap_or_default())
                    .map_err(errno_for_fade)?;
            } else if !self.parent_directory_visible(&path) {
                return Err(EINVAL);
            }

            let backing_path = self.backing_path(&path)?;
            fs::create_dir(&backing_path).map_err(errno_for_io)?;
            let permissions = (mode & !umask) & 0o7777;
            fs::set_permissions(&backing_path, fs::Permissions::from_mode(permissions))
                .map_err(errno_for_io)?;
            self.visible_attr(&path)
        })();

        match result {
            Ok(attr) => reply.entry(&ATTR_TTL, &attr, 0),
            Err(errno) => reply.error(errno),
        }
    }

    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let result = (|| {
            let path = self.child_path(parent, name)?;
            if !self.parent_directory_visible(&path) {
                return Err(EINVAL);
            }

            let assignment = self.policy.assign_file(&path).map_err(errno_for_fade)?;
            let backing_path = self.backing_path(&path)?;
            let file = open_options(flags, true, (mode & !umask) & 0o7777)
                .open(&backing_path)
                .map_err(errno_for_io)?;

            let record = FileRecord::new(
                path.clone(),
                path.clone(),
                assignment.ttl,
                assignment.source.as_label(),
                now_unix_seconds(),
                self.recovery_window,
                0,
            )
            .map_err(errno_for_fade)?;

            if let Err(error) = self.store.insert_file(&record) {
                let _ = fs::remove_file(&backing_path);
                return Err(errno_for_fade(error));
            }

            let attr = self.visible_attr(&path)?;
            let fh = self.open_handle(file, path);
            Ok((attr, fh))
        })();

        match result {
            Ok((attr, fh)) => reply.created(&ATTR_TTL, &attr, 0, fh, flags as u32),
            Err(errno) => reply.error(errno),
        }
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        let result = (|| {
            let path = self.path_for_ino(ino)?;
            self.visible_file_record(&path)?;
            let backing_path = self.backing_path(&path)?;
            let file = open_options(flags, false, 0)
                .open(backing_path)
                .map_err(errno_for_io)?;
            Ok(self.open_handle(file, path))
        })();

        match result {
            Ok(fh) => reply.opened(fh, 0),
            Err(errno) => reply.error(errno),
        }
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let result = (|| {
            if offset < 0 {
                return Err(EINVAL);
            }

            let path = self
                .open_handles
                .get(&fh)
                .map(|handle| handle.path.clone())
                .ok_or(EBADF)?;
            self.visible_file_record(&path)?;

            let handle = self.open_handles.get_mut(&fh).ok_or(EBADF)?;
            handle
                .file
                .seek(SeekFrom::Start(offset as u64))
                .map_err(errno_for_io)?;
            let mut buffer = vec![0; size as usize];
            let read = handle.file.read(&mut buffer).map_err(errno_for_io)?;
            buffer.truncate(read);
            Ok(buffer)
        })();

        match result {
            Ok(buffer) => reply.data(&buffer),
            Err(errno) => reply.error(errno),
        }
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let result = (|| {
            if offset < 0 {
                return Err(EINVAL);
            }

            let path = self
                .open_handles
                .get(&fh)
                .map(|handle| handle.path.clone())
                .ok_or(EBADF)?;
            self.visible_file_record(&path)?;

            let handle = self.open_handles.get_mut(&fh).ok_or(EBADF)?;
            handle
                .file
                .seek(SeekFrom::Start(offset as u64))
                .map_err(errno_for_io)?;
            let written = handle.file.write(data).map_err(errno_for_io)?;
            self.refresh_observed_size(&path)?;
            Ok(written as u32)
        })();

        match result {
            Ok(written) => reply.written(written),
            Err(errno) => reply.error(errno),
        }
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        if let Some(handle) = self.open_handles.remove(&fh) {
            let _ = self.refresh_observed_size(&handle.path);
        }
        reply.ok();
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let result = (|| {
            let path = self.path_for_ino(ino)?;
            let backing_path = self.backing_path(&path)?;
            let mut entries = Vec::new();
            entries.push((ino, FileType::Directory, OsString::from(".")));
            let parent_ino = path
                .parent()
                .map(|parent| self.inode_for_path(&parent))
                .unwrap_or(ROOT_INO);
            entries.push((parent_ino, FileType::Directory, OsString::from("..")));

            let mut children = fs::read_dir(backing_path)
                .map_err(errno_for_io)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(errno_for_io)?;
            children.sort_by_key(|entry| entry.file_name());

            for child in children {
                let name = child.file_name();
                if name == FADE_DIR {
                    continue;
                }

                let Some(name_str) = name.to_str() else {
                    continue;
                };
                let child_path = match path.join_segment(name_str) {
                    Ok(path) => path,
                    Err(_) => continue,
                };
                let metadata = match fs::symlink_metadata(child.path()) {
                    Ok(metadata) => metadata,
                    Err(_) => continue,
                };
                let Some(kind) = file_type(&metadata) else {
                    continue;
                };

                if kind == FileType::Directory {
                    if !self.directory_visible(&child_path) {
                        continue;
                    }
                } else if self.visible_file_record(&child_path).is_err() {
                    continue;
                }

                let child_ino = self.inode_for_path(&child_path);
                entries.push((child_ino, kind, name));
            }

            Ok(entries)
        })();

        match result {
            Ok(entries) => {
                for (index, (entry_ino, kind, name)) in entries.into_iter().enumerate() {
                    if index < offset.max(0) as usize {
                        continue;
                    }

                    if reply.add(entry_ino, (index + 1) as i64, kind, name) {
                        break;
                    }
                }
                reply.ok();
            }
            Err(errno) => reply.error(errno),
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let result = (|| {
            let path = self.child_path(parent, name)?;
            self.visible_file_record(&path)?;
            let backing_path = self.backing_path(&path)?;
            fs::remove_file(backing_path).map_err(errno_for_io)?;
            self.store
                .mark_deleted_by_path(&path)
                .map_err(errno_for_fade)?;
            self.remove_inode_for_path(&path);
            Ok(())
        })();

        match result {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let result = (|| {
            let path = self.child_path(parent, name)?;
            if !self.directory_visible(&path) {
                return Err(ENOENT);
            }

            let backing_path = self.backing_path(&path)?;
            fs::remove_dir(backing_path).map_err(errno_for_io)?;
            self.remove_inode_for_path(&path);
            Ok(())
        })();

        match result {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        flags: u32,
        reply: ReplyEmpty,
    ) {
        let result = (|| {
            if flags != 0 {
                return Err(ENOSYS);
            }

            let from = self.child_path(parent, name)?;
            let to = self.child_path(newparent, newname)?;
            if !self.parent_directory_visible(&to) {
                return Err(EINVAL);
            }

            let from_backing = self.backing_path(&from)?;
            let to_backing = self.backing_path(&to)?;
            if to_backing.exists() {
                return Err(EEXIST);
            }

            let metadata = fs::symlink_metadata(&from_backing).map_err(errno_for_io)?;
            let kind = file_type(&metadata).ok_or(ENOENT)?;
            if kind == FileType::Directory {
                if !self.directory_visible(&from) {
                    return Err(ENOENT);
                }
            } else {
                self.visible_file_record(&from)?;
            }

            fs::rename(&from_backing, &to_backing).map_err(errno_for_io)?;
            if kind == FileType::Directory {
                self.store
                    .rename_prefix(&from, &to, now_unix_seconds())
                    .map_err(errno_for_fade)?;
            } else {
                self.store
                    .rename_path(&from, &to, now_unix_seconds())
                    .map_err(errno_for_fade)?;
            }
            self.rename_inode_paths(&from, &to);
            Ok(())
        })();

        match result {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }
}

fn open_options(flags: i32, create_new: bool, mode: u32) -> OpenOptions {
    let mut options = OpenOptions::new();
    match flags & O_ACCMODE {
        O_RDONLY => {
            options.read(true);
        }
        O_WRONLY => {
            options.write(true);
        }
        O_RDWR => {
            options.read(true).write(true);
        }
        _ => {
            options.read(true);
        }
    };

    if flags & O_APPEND != 0 {
        options.append(true);
    }
    if flags & O_TRUNC != 0 {
        options.truncate(true);
    }
    if create_new {
        options.create_new(true).mode(mode);
    }

    options
}

fn file_attr(ino: u64, metadata: &fs::Metadata) -> FileAttr {
    FileAttr {
        ino,
        size: metadata.len(),
        blocks: metadata.blocks(),
        atime: unix_time(metadata.atime(), metadata.atime_nsec()),
        mtime: unix_time(metadata.mtime(), metadata.mtime_nsec()),
        ctime: unix_time(metadata.ctime(), metadata.ctime_nsec()),
        crtime: unix_time(metadata.ctime(), metadata.ctime_nsec()),
        kind: file_type(metadata).unwrap_or(FileType::RegularFile),
        perm: (metadata.mode() & 0o7777) as u16,
        nlink: metadata.nlink() as u32,
        uid: metadata.uid(),
        gid: metadata.gid(),
        rdev: metadata.rdev() as u32,
        blksize: metadata.blksize() as u32,
        flags: 0,
    }
}

fn file_type(metadata: &fs::Metadata) -> Option<FileType> {
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        Some(FileType::Directory)
    } else if file_type.is_file() {
        Some(FileType::RegularFile)
    } else {
        None
    }
}

fn unix_time(seconds: i64, nanos: i64) -> SystemTime {
    if seconds < 0 {
        return UNIX_EPOCH;
    }

    let nanos = nanos.clamp(0, 999_999_999) as u32;
    UNIX_EPOCH + Duration::new(seconds as u64, nanos)
}

fn errno_for_fade(error: FadeError) -> i32 {
    debug!(%error, "Fade operation failed");
    match error {
        FadeError::InvalidDuration { .. } | FadeError::InvalidPath { .. } => EINVAL,
        FadeError::MissingTtl { .. } => EINVAL,
        FadeError::PathTraversal { .. } => EACCES,
        FadeError::Io(error) => errno_for_io(error),
        FadeError::Sqlite(_) => EIO,
        FadeError::InvalidState(_) | FadeError::InvalidSize(_) | FadeError::TimeOverflow => EIO,
    }
}

fn errno_for_io(error: std::io::Error) -> i32 {
    debug!(%error, "filesystem operation failed");
    match error.kind() {
        std::io::ErrorKind::NotFound => ENOENT,
        std::io::ErrorKind::PermissionDenied => EACCES,
        std::io::ErrorKind::AlreadyExists => EEXIST,
        std::io::ErrorKind::InvalidInput => EINVAL,
        std::io::ErrorKind::NotADirectory => ENOTDIR,
        std::io::ErrorKind::IsADirectory => EISDIR,
        _ => EIO,
    }
}

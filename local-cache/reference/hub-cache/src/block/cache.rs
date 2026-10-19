// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

// TODO: This is for RecordV1. For whatever reason, annotating the struct
//       itself doesn't work. Maybe something to do with traits? Try to get
//       this working again later.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashSet};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::SystemTime;
use std::{fmt, fs};

use bitflags::bitflags;
use bytemuck::{Pod, Zeroable};
use bytesize::ByteSize;
use dashmap::{mapref::entry::Entry, DashMap};
use semver::Version;
use serde::{Deserialize, Serialize};
use sled::IVec;
use tokio::{
    sync::{mpsc, Mutex, Notify, OnceCell, Semaphore},
    task::JoinSet,
    time::{sleep, Duration, Instant},
};
use tracing::{debug, error, info, instrument, trace, warn};
use uuid::Uuid;

use hub_util::fs::remove_file;
use hub_util::HexSlice;

use super::file::FilePool;
use super::record;
use super::{AtomicCell, Error, Result};

//
// TODOs
//
// - size .. cache.size only accounts for the conceptual size of the cache,
//           but not the data files, which could be fragmented. need to track both entries size and data files size
//           there is some difficultly in keeping on disk data files size accurate. since files are allocated with
//           size == 0, only later after the file is closed, is the file on disk stat()'d and its true/final size known
//

#[derive(Debug)]
pub struct CacheBuilder {
    /// path to database
    path: PathBuf,
    /// maximum size of cache
    max_size: ByteSize,
    /// percent of `max_size` that garbage collection should try and clean up before stopping
    garbage_collection_threshold: u64,
    /// garbage collection interval
    garbage_collection_interval: Duration,
    /// maximum size of the page cache
    page_cache_capacity: ByteSize,
    /// use compression for db
    use_compression: bool,
    /// zstd compression. ranges from 1 up to 22. levels >= 20 are ‘ultra’.
    compression_factor: i32,
    /// whether database should be temporary (memory only)
    temporary: bool,
    /// size of the block data files
    data_file_size: ByteSize,
}

impl CacheBuilder {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_owned(),
            max_size: ByteSize::gib(1),
            garbage_collection_threshold: 10,
            garbage_collection_interval: Duration::from_secs(900),
            page_cache_capacity: ByteSize::mib(128),
            use_compression: true,
            compression_factor: 1,
            temporary: false,
            data_file_size: ByteSize::mib(64),
        }
    }

    pub fn max_size(mut self, max_size: impl Into<ByteSize>) -> Result<Self> {
        let max_size = max_size.into();

        if max_size.as_u64() == 0 {
            return Err(Error::InvalidArgument("Cache max size must be > 0".to_owned()));
        }

        self.max_size = max_size;
        Ok(self)
    }

    pub fn garbage_collection_threshold(mut self, threshold: impl Into<u64>) -> Result<Self> {
        let threshold = threshold.into();

        if threshold > 100 {
            return Err(Error::InvalidGarbageCollectionThreshold(threshold));
        }

        self.garbage_collection_threshold = threshold;
        Ok(self)
    }

    pub fn garbage_collection_interval(mut self, interval: impl Into<Duration>) -> Result<Self> {
        let interval = interval.into();

        if interval.is_zero() {
            return Err(Error::InvalidGarbageCollectionInterval);
        }

        self.garbage_collection_interval = interval;
        Ok(self)
    }

    pub fn page_cache_capacity(mut self, capacity: impl Into<ByteSize>) -> Self {
        self.page_cache_capacity = capacity.into();
        self
    }

    pub fn use_compression(mut self, compression: bool) -> Self {
        self.use_compression = compression;
        self
    }

    pub fn compression_factor(mut self, compression_factor: impl Into<i32>) -> Result<Self> {
        let compression_factor = compression_factor.into();

        if !(1..=22).contains(&compression_factor) {
            return Err(Error::InvalidCompressionFactor(compression_factor));
        }

        self.compression_factor = compression_factor;
        Ok(self)
    }

    pub fn temporary(mut self, temporary: bool) -> Self {
        self.temporary = temporary;
        self
    }

    pub fn data_file_size(mut self, data_file_size: impl Into<ByteSize>) -> Self {
        self.data_file_size = data_file_size.into();
        self
    }

    pub fn open(self) -> Result<Cache> {
        let path = fs::canonicalize(&self.path)?;
        info!(?path, "cache starting");

        // base directory
        fs::create_dir_all(&path)?;

        // open database
        let db = sled::Config::new()
            .cache_capacity(self.page_cache_capacity.as_u64())
            .mode(sled::Mode::LowSpace)
            .use_compression(self.use_compression)
            .compression_factor(self.compression_factor)
            .temporary(self.temporary)
            .path(path.join("hub-db"))
            .open()?;

        // open key/value tables
        let system = db.open_tree("system")?;
        let index = db.open_tree("index")?;

        // look for database uuid
        let uuid = match system.get("uuid")? {
            Some(entry) => bincode::deserialize(entry.as_ref())?,
            None => {
                let uuid = Uuid::new_v4();
                system.insert("uuid", bincode::serialize(&uuid)?)?;
                uuid
            },
        };
        assert!(!uuid.is_nil());

        // look for on disk format verison
        let version = match system.get("version")? {
            Some(entry) => bincode::deserialize(entry.as_ref())?,
            None => {
                let version = Version::new(1, 0, 0);
                system.insert("version", bincode::serialize(&version)?)?;
                version
            },
        };
        // for now we have just the v1.0.0 format, in the future we may need to perform db upgrades here
        assert_eq!(version, Version::new(1, 0, 0));

        // flush this out to disk
        system.flush()?;

        let open_records = DashMap::new();
        let mut entries = 0;
        let mut size = 0;

        for entry in index.iter() {
            let entry = entry?;
            let key = Arc::from(entry.0.as_ref());
            let record = bytemuck::from_bytes::<RecordV1>(&entry.1);
            debug!(key = %HexSlice::new(&key), %record);

            entries += 1;
            size += record.data_size;

            // if we find a record that is not committed on startup, add to open records as abandoned
            // if some client does not come along and claim it, eventually GC will reclaim/remove it
            if !record.flags.contains(FlagsV1::COMMITTED) {
                info!(key = %HexSlice::new(&key), %record, "found abandoned record");
                let previous = open_records.insert(
                    key,
                    OpenRecord::Abandoned {
                        value: entry.1,
                        gc_intervals: 0,
                    },
                );
                assert!(previous.is_none());
            }
        }

        let file_pool = FilePool::new(&path, self.data_file_size.as_u64())?;

        info!(?path, %uuid, %version, entries, size = bytesize::ByteSize::b(size).to_string_as(true), "cache started");

        // create cache
        let cache = Cache(Arc::new(CacheInner {
            uuid,
            version,
            started: Instant::now(),
            now: AtomicCell::new(Cache::now()?),
            path,
            db,
            _system: system,
            index,
            entries: AtomicU64::new(entries),
            size: AtomicU64::new(size),
            stats: InternalStats::new(),
            file_pool,
            open_records,
            lru_channel: OnceCell::new(),
            max_size: AtomicCell::new(self.max_size.as_u64()),
            garbage_collection_lock: Arc::new(Semaphore::new(1)),
            garbage_collection_notify: Arc::new(Notify::new()),
            garbage_collection_interval: AtomicCell::new(self.garbage_collection_interval),
            garbage_collection_threshold: AtomicCell::new(self.garbage_collection_threshold),
        }));

        Ok(cache)
    }
}

#[derive(Debug, Clone)]
pub struct Cache(pub(crate) Arc<CacheInner>);

impl Deref for Cache {
    type Target = CacheInner;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Open Record
///  1. Closed - record is not open (implicit, as no record exists in open records map)
///  2. Get - record is open for get by one or more clients
///  3. Put - record is open for put by one client
///  4. Abandoned - record was open for put, but client disconnected or cache restarted before being committed
#[derive(Debug)]
enum OpenRecord {
    // use_count: reference count, location: location of data on disk (file, offset, size)
    Get { use_count: u64, location: Location },
    // value: database record, extension: optional file extension
    Put { value: IVec, extension: Option<Arc<str>> },
    // value: database record, gc_intervals: number of gc run since record became abandoned
    Abandoned { value: IVec, gc_intervals: u64 },
}

#[derive(Debug)]
pub struct CacheInner {
    /// cache unique id
    uuid: Uuid,
    /// version of the cache
    version: Version,
    /// instant cache was started
    started: Instant,
    /// cached now (epoch) for last accessed
    now: AtomicCell<u64>,
    /// base directory
    path: PathBuf,
    /// backing database for cache metadata
    db: sled::Db,
    /// key/value table of system data
    _system: sled::Tree,
    /// key/value table of all the key -> block data / file data
    index: sled::Tree,
    /// total number of entries in cache (sled len() is O(n) operation)
    entries: AtomicU64,
    /// total size of the cache in bytes
    size: AtomicU64,
    /// internal cache statistics
    stats: InternalStats,
    /// pool of files, active (in-use) and pending
    file_pool: FilePool,
    /// in-memory use count of open get()/put() calls, maps key -> use count
    open_records: DashMap<Arc<[u8]>, OpenRecord>,
    /// LRU background update send channel
    lru_channel: OnceCell<LRUChannel>,
    /// maximum size of cache on disk in bytes
    max_size: AtomicCell<u64>,
    /// garbage collection semaphore to only allow 1 gc operation at one time
    garbage_collection_lock: Arc<Semaphore>,
    /// notify garbage collection to run at next chance, do not wait for timeout
    garbage_collection_notify: Arc<Notify>,
    /// garbage collection interval
    garbage_collection_interval: AtomicCell<Duration>,
    /// percent of `max_size` that garbage collection should try and clean up before stopping (0-100)
    garbage_collection_threshold: AtomicCell<u64>,
}

impl Drop for CacheInner {
    fn drop(&mut self) {
        self.garbage_collection_lock.close();
    }
}

impl Cache {
    pub fn builder(path: impl AsRef<Path>) -> CacheBuilder {
        CacheBuilder::new(path)
    }

    /// get the current system time in seconds from epoch
    /// used for LRU / last accessed times for garbage collection
    /// so clock drift is not really a critical issue, if clock were
    /// changed while hub is running, it may result in skewed GC
    /// behavior, but nothing serious.
    fn now() -> Result<u64> {
        Ok(SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?.as_secs())
    }

    #[instrument(level = "info", skip_all, err, fields(max_size))]
    fn set_max_size(&self, max_size: impl Into<ByteSize>) -> Result<()> {
        let max_size = max_size.into();
        tracing::Span::current().record("max_size", max_size.as_u64());
        self.max_size.set(max_size.as_u64());
        Ok(())
    }

    #[instrument(level = "debug", skip(self), err)]
    fn connect_get(&self, request: &ConnectRequest) -> Result<()> {
        if request.key.is_empty() {
            return Err(Error::KeyEmpty);
        }

        match self.open_records.entry(request.key.clone()) {
            Entry::Occupied(mut entry) => match entry.get_mut() {
                OpenRecord::Get { use_count, .. } => {
                    // key exists and is already open for get
                    *use_count += 1;
                    Ok(())
                },
                OpenRecord::Put { .. } => {
                    trace!(key = %HexSlice::new(entry.key()), "open for put");
                    Err(Error::KeyOpenForPut(format!("{}", HexSlice::new(entry.key()))))
                },
                OpenRecord::Abandoned { .. } => {
                    trace!(key = %HexSlice::new(entry.key()), "not found");
                    Err(Error::KeyNotFound(format!("{}", HexSlice::new(entry.key()))))
                },
            },
            Entry::Vacant(entry) => match self.index.get(entry.key())? {
                Some(value) => {
                    // TODO: similar code to get1(), could consider consolidating in the future
                    let record = bytemuck::from_bytes::<RecordV1>(&value);
                    assert!(record.flags.contains(FlagsV1::COMMITTED));

                    let location = Location {
                        path: FilePool::file_path(record.data_file, &request.extension),
                        id: record.data_file,
                        offset: record.data_offset,
                        size: record.data_size,
                    };

                    // queue lru update
                    if let Some(channel) = self.lru_channel.get() {
                        channel.tx.send((entry.key().clone(), self.now.get()))?;
                    }

                    entry.insert(OpenRecord::Get { use_count: 1, location });

                    self.stats.hit();
                    Ok(())
                },
                None => {
                    trace!(key = %HexSlice::new(entry.key()), "not found");
                    Err(Error::KeyNotFound(format!("{}", HexSlice::new(entry.key()))))
                },
            },
        }
    }

    #[instrument(level = "debug", skip(self), err)]
    fn connect_put(&self, request: &ConnectRequest, client_uuid: &Uuid) -> Result<()> {
        // key validation
        if request.key.is_empty() {
            return Err(Error::KeyEmpty);
        }

        match self.open_records.entry(request.key.clone()) {
            Entry::Occupied(mut entry) => match entry.get() {
                OpenRecord::Get { .. } => {
                    trace!(key = %HexSlice::new(entry.key()), "already exists");
                    Err(Error::KeyAlreadyExists(format!("{}", HexSlice::new(entry.key()))))
                },
                OpenRecord::Put { .. } => {
                    trace!(key = %HexSlice::new(entry.key()), "open for put");
                    Err(Error::KeyOpenForPut(format!("{}", HexSlice::new(entry.key()))))
                },
                OpenRecord::Abandoned { value, .. } => {
                    // TODO: ConnectRequest needs at file/offset/size to be included in order to
                    //       re-use the existing index record, if they don't match or client uuid
                    //       does not match, we must allocate a new data location

                    let record = bytemuck::from_bytes::<RecordV1>(value);
                    assert!(!record.flags.contains(FlagsV1::COMMITTED));

                    //  need to check if we're the client that was writing it
                    if &record.client_uuid == client_uuid.as_bytes() {
                        entry.insert(OpenRecord::Put {
                            value: value.clone(),
                            extension: request.extension.clone(),
                        });
                        Ok(())
                    } else {
                        trace!(key = %HexSlice::new(entry.key()), "abandoned put");
                        Err(Error::BlockAbandoned(format!("{}", HexSlice::new(entry.key()))))
                    }
                },
            },
            Entry::Vacant(entry) => match self.index.get(entry.key())? {
                Some(value) => {
                    // we should never have a record not COMMITTED here, as any abandoned records
                    // should be read at startup and placed in the open records map as abandoned
                    let record = bytemuck::from_bytes::<RecordV1>(&value);
                    trace!(key = %HexSlice::new(entry.key()), %record, "already exists");
                    assert!(record.flags.contains(FlagsV1::COMMITTED));
                    Err(Error::KeyAlreadyExists(format!("{}", HexSlice::new(entry.key()))))
                },
                None => {
                    trace!(key = %HexSlice::new(entry.key()), "not found");
                    Err(Error::KeyNotFound(format!("{}", HexSlice::new(entry.key()))))
                },
            },
        }
    }

    /// Reopen keys when a client reconnects
    #[instrument(level = "debug", skip_all, fields(bytes))]
    pub fn connect(&self, requests: &[ConnectRequest], client_id: &Uuid) -> Vec<Result<()>> {
        requests
            .iter()
            .map(|request| match request.open_for_put {
                true => self.connect_put(request, client_id),
                false => self.connect_get(request),
            })
            .collect()
    }

    /// internal stat operation for a single key
    #[instrument(level = "debug", err, skip(self, now))]
    fn stat1(&self, key: &Arc<[u8]>, now: u64) -> Result<Option<Metadata>> {
        if key.is_empty() {
            return Err(Error::KeyEmpty);
        }

        Ok(
            match self
                .index
                .get(key)?
                .as_ref()
                .map(AsRef::as_ref)
                .map(bytemuck::from_bytes::<RecordV1>)
                // only fully committed records, ie not in the middle of a put()
                .filter(|record| record.flags.contains(FlagsV1::COMMITTED))
            {
                Some(record) => {
                    if let Some(channel) = self.lru_channel.get() {
                        channel.tx.send((key.clone(), now))?;
                    }

                    Some(Metadata {
                        // TODO: should this send back "now" (even tho its not updated until later in background)???
                        // TODO: what about accesses count as well, since both it and last_access are updated in background
                        // TODO: should we just remove them and only return size?
                        last_accessed: record.last_accessed,
                        accesses: record.accesses,
                        size: record.data_size,
                    })
                },
                None => None,
            },
        )
    }

    /// stat a slice of keys, returning the results as a vector. None means the key was not found.
    #[instrument(level = "debug", skip_all, fields(len = keys.len(), now))]
    pub fn block_stat(&self, keys: &[Arc<[u8]>]) -> Vec<Result<Option<Metadata>>> {
        let now = self.now.get();
        record!(now);
        // flush not needed here, last_access updated, but not necessary for crash recovery
        keys.iter().map(|key| self.stat1(key, now)).collect()
    }

    /// internal put operation for a single key
    #[instrument(level = "debug", skip_all, ret, fields(key = %HexSlice::new(&request.key), client_uuid))]
    fn put1(&self, request: &PutRequest, now: u64, client_uuid: &Uuid) -> Result<Location> {
        // key is not empty (sled allows this)
        if request.key.is_empty() {
            return Err(Error::KeyEmpty);
        }

        // make sure something to store (though allowing zero-length data could exist)
        if request.size == 0 {
            return Err(Error::InvalidArgument("Request size must be > 0".to_owned()));
        }

        // check key isn't larger than max cache size
        if request.size > self.max_size.get() {
            return Err(Error::KeyTooLarge(
                request.key.clone(),
                request.size,
                self.max_size.get(),
            ));
        }

        match self.open_records.entry(request.key.clone()) {
            Entry::Occupied(entry) => {
                // key is in open records map, the key already exists, either is partial put()
                // or there is at least one or more open get() calls already...
                match entry.get() {
                    OpenRecord::Get { .. } => Err(Error::KeyAlreadyExists(format!("{}", HexSlice::new(entry.key())))),
                    OpenRecord::Put { .. } => {
                        self.stats.put_during_put();
                        Err(Error::KeyOpenForPut(format!("{}", HexSlice::new(entry.key()))))
                    },
                    OpenRecord::Abandoned { value, .. } => {
                        // abandoned record,
                        // check before allocating new data location:
                        //   - client uuid
                        //   - size
                        //   - file exists
                        let record = bytemuck::from_bytes::<RecordV1>(value);
                        let path = FilePool::file_path(record.data_file, &request.extension);

                        let location = if client_uuid.as_bytes() == &record.client_uuid
                            && request.size == record.data_size
                            && path.exists()
                        {
                            Location {
                                path,
                                id: record.data_file,
                                offset: record.data_offset,
                                size: record.data_size,
                            }
                        } else {
                            // cannot re-use existing file, so allocate new one
                            let data_location = self.file_pool.next_data_location(request.size, &request.extension)?;
                            Location {
                                path: FilePool::file_path(data_location.id(), &request.extension),
                                id: data_location.id(),
                                offset: data_location.offset(),
                                size: request.size,
                            }
                        };
                        trace!(?location);

                        // convert open entry from abandoned to put
                        let put_record = OpenRecord::Put {
                            value: value.clone(),
                            extension: request.extension.clone(),
                        };
                        entry.replace_entry(put_record);

                        Ok(location)
                    },
                }
            },
            Entry::Vacant(entry) => {
                // entry holds the lock for us, important it does not get inserted into map
                // until after all data file and index operations have completed

                // the record maybe in the index, but not open, so we need to check the index first
                match self.index.get(entry.key())? {
                    Some(_) => {
                        trace!(key = %HexSlice::new(entry.key()), "already exists");
                        Err(Error::KeyAlreadyExists(format!("{}", HexSlice::new(entry.key()))))
                    },
                    None => {
                        // get location to store the block
                        let data_location = self.file_pool.next_data_location(request.size, &request.extension)?;
                        trace!(?data_location);

                        // if put is requested for unique file, mark it so
                        // this flag is strictly necessary, but helpful for debugging
                        let flags = if request.extension.is_some() {
                            FlagsV1::FILE
                        } else {
                            FlagsV1::empty()
                        };

                        // setup index record
                        let record = RecordV1 {
                            flags,
                            data_file: data_location.id(),
                            data_offset: data_location.offset(),
                            data_size: request.size,
                            last_accessed: now,
                            accesses: 0,
                            rank: 0,
                            client_uuid: *client_uuid.as_bytes(),
                            reserved0: 0,
                            reserved1: [0u8; 32],
                        };

                        // make a copy of the record to store in the open records map since sled consumes it
                        // will make it easier to update the record when its committed / closed
                        let value = IVec::from(&record);

                        let previous = self.index.insert(entry.key(), value.clone())?;
                        assert!(previous.is_none());

                        entry.insert(OpenRecord::Put {
                            value,
                            extension: request.extension.clone(),
                        });

                        Ok(Location {
                            path: FilePool::file_path(data_location.id(), &request.extension),
                            id: data_location.id(),
                            offset: data_location.offset(),
                            size: request.size,
                        })
                    },
                }
            },
        }
    }

    /// start put operations for a slice of keys returning a `PutResult`
    #[instrument(level = "debug", skip_all, err, fields(len = requests.len(), now))]
    pub fn block_put(&self, requests: &[PutRequest], client_id: &Uuid) -> Result<PutResult> {
        let now = self.now.get();
        record!(now);
        let result = PutResult {
            base: self.file_pool.base_path().to_owned(),
            locations: requests
                .iter()
                .map(|request| self.put1(request, now, client_id))
                .collect(),
        };
        self.flush()?;
        Ok(result)
    }

    /// commit a put operation for a given `handle`. after this method returns, key is officially part of the cache
    #[instrument(level = "debug", err, skip(self, now))]
    fn commit1_deprecated(&self, key: &Arc<[u8]>, now: u64) -> Result<()> {
        //
        // committing key involves:
        //  *) updating last_accessed field
        //  *) setting the committed bit in the flags field
        //  *) increasing the cache size record
        //  *) removing from open records map
        //
        //  note: individual commit1() operations are not flushed to disk.
        //        the bulk commit() operation will issue a single flush
        //        after all keys in the batch were committed
        //

        match self.open_records.entry(key.clone()) {
            Entry::Occupied(entry) => match entry.get() {
                OpenRecord::Get { .. } => Err(Error::KeyAlreadyExists(format!("{}", HexSlice::new(entry.key())))),
                OpenRecord::Put { value, .. } => {
                    let mut record = *bytemuck::from_bytes::<RecordV1>(value);
                    record.last_accessed = now;
                    record.flags.insert(FlagsV1::COMMITTED);

                    // override the record with the latest, check that there was at least an entry in the index
                    let previous = self.index.insert(entry.key(), record)?;
                    assert!(previous.is_some());

                    // record increased cache size
                    self.entries.fetch_add(1, Ordering::Relaxed);
                    self.size.fetch_add(record.data_size, Ordering::Relaxed);

                    // stats on total data stored in cache
                    self.stats.data_stored(record.data_size);

                    // in old API, once a record is committed, its also closed
                    entry.remove();
                    Ok(())
                },
                OpenRecord::Abandoned { .. } => Err(Error::KeyNotFound(format!("{}", HexSlice::new(entry.key())))),
            },
            Entry::Vacant(entry) => {
                warn!(key = %HexSlice::new(entry.key()), "commit key not found in open records map");
                Err(Error::KeyNotFound(format!("{}", HexSlice::new(entry.key()))))
            },
        }
    }

    /// commit put operations for a slice of handles, returning the results as a vector.
    #[instrument(level = "debug", skip_all, err, fields(len = keys.len(), now))]
    pub fn block_commit_deprecated(&self, keys: &[Arc<[u8]>]) -> Result<Vec<Result<()>>> {
        let now = self.now.get();
        record!(now);
        let result = keys.iter().map(|key| self.commit1_deprecated(key, now)).collect();
        self.flush()?;
        Ok(result)
    }

    /// abort a put operation for a given `handle`
    #[instrument(level = "debug", skip(self), err)]
    fn abort1_deprecated(&self, key: &Arc<[u8]>) -> Result<()> {
        match self.open_records.entry(key.clone()) {
            Entry::Occupied(entry) => match entry.get() {
                OpenRecord::Get { .. } => Err(Error::KeyAlreadyExists(format!("{}", HexSlice::new(entry.key())))),
                OpenRecord::Put { .. } => {
                    // remove from index
                    self.index.remove(entry.key())?;

                    // remove from open map, use count must be 1
                    entry.remove();
                    Ok(())
                },
                OpenRecord::Abandoned { .. } => Err(Error::KeyNotFound(format!("{}", HexSlice::new(entry.key())))),
            },
            Entry::Vacant(entry) => {
                warn!(key = %HexSlice::new(entry.key()), "commit key not found in open records map");
                Err(Error::KeyNotFound(format!("{}", HexSlice::new(entry.key()))))
            },
        }
    }

    /// abort put operations for a slice of handles, returning the results as a vector.
    #[instrument(level = "debug", skip_all, err, fields(len = keys.len()))]
    pub fn block_abort_deprecated(&self, keys: &[Arc<[u8]>]) -> Result<Vec<Result<()>>> {
        let result = keys.iter().map(|key| self.abort1_deprecated(key)).collect();
        self.flush()?;
        Ok(result)
    }

    /// abandon a put operation, call this over abort if you want to leave the ability for a client
    /// to restart the put operation. after grace period is over, typically some number of garbage collection
    /// intervals, garbage collection will abort/remove abandon records
    #[instrument(level = "debug", err, skip_all)]
    fn abandon1(&self, key: &Arc<[u8]>) -> Result<()> {
        match self.open_records.entry(key.clone()) {
            Entry::Occupied(entry) => match entry.get() {
                OpenRecord::Get { .. } => Err(Error::KeyAlreadyExists(format!("{}", HexSlice::new(entry.key())))),
                OpenRecord::Put { value, .. } => {
                    let abandoned_record = OpenRecord::Abandoned {
                        value: value.clone(),
                        gc_intervals: 0,
                    };
                    entry.replace_entry(abandoned_record);

                    Ok(())
                },
                OpenRecord::Abandoned { .. } => Ok(()), // already abandoned
            },
            Entry::Vacant(entry) => {
                warn!(key = %HexSlice::new(entry.key()), "key not found in open records map to abandon");
                Err(Error::KeyNotFound(format!("{}", HexSlice::new(entry.key()))))
            },
        }
    }

    /// commit put operations for a slice of handles, returning the results as a vector.
    #[instrument(level = "debug", skip_all, err, fields(len = keys.len()))]
    pub fn block_abandon(&self, keys: &[Arc<[u8]>]) -> Result<Vec<Result<()>>> {
        let result = keys.iter().map(|key| self.abandon1(key)).collect();
        self.flush()?;
        Ok(result)
    }

    /// get cache entry for key
    #[instrument(level = "debug", err, skip(self, now))]
    fn get1(&self, key: &Arc<[u8]>, extension: Option<Arc<str>>, now: u64) -> Result<Option<Location>> {
        if key.is_empty() {
            return Err(Error::KeyEmpty);
        }

        self.stats.access();

        Ok(match self.open_records.entry(key.clone()) {
            Entry::Occupied(mut entry) => {
                // key found in open records
                let open_record = entry.get_mut();

                match open_record {
                    // key is already open for get
                    OpenRecord::Get { use_count, location } => {
                        // queue lru update
                        if let Some(channel) = self.lru_channel.get() {
                            channel.tx.send((key.clone(), now))?;
                        }

                        // inc use count for this key
                        *use_count += 1;

                        self.stats.hit();
                        Some(location.clone())
                    },
                    OpenRecord::Put { .. } => {
                        trace!(key = %HexSlice::new(entry.key()), "key open for put");
                        self.stats.get_during_put();
                        return Err(Error::KeyOpenForPut(format!("{}", HexSlice::new(entry.key()))));
                    },
                    OpenRecord::Abandoned { .. } => {
                        trace!(key = %HexSlice::new(entry.key()), "key abandoned");
                        None // abandoned key is treated like not found
                    },
                }
            },
            Entry::Vacant(entry) => {
                // key not found in open records, so we must look it up in the index
                match self.index.get(key)? {
                    Some(value) => {
                        let record = bytemuck::from_bytes::<RecordV1>(value.as_ref());
                        // we should never we returning an uncommitted record
                        assert!(record.flags.contains(FlagsV1::COMMITTED));

                        // TODO: make function for this
                        let location = Location {
                            path: FilePool::file_path(record.data_file, &extension),
                            id: record.data_file,
                            offset: record.data_offset,
                            size: record.data_size,
                        };

                        // queue lru update
                        if let Some(channel) = self.lru_channel.get() {
                            channel.tx.send((key.clone(), now))?;
                        }

                        // add this record to the open records map
                        entry.insert(OpenRecord::Get {
                            use_count: 1,
                            location: location.clone(),
                        });

                        self.stats.hit();
                        Some(location)
                    },
                    None => {
                        trace!(key = %HexSlice::new(entry.key()), "not found");
                        None // key not found in cache
                    },
                }
            },
        })
    }

    /// get a slice of keys, returning the results as a vector. None means the key was not found.
    #[instrument(level = "debug", skip_all, fields(len = keys.len(), now))]
    pub fn block_get(&self, keys: &[Arc<[u8]>]) -> GetResult {
        let now = self.now.get();
        record!(now);

        // flush is not needed, only use_count/last_access are updated and are not necessary for crash recovery
        GetResult {
            base: self.file_pool.base_path().to_owned(),
            locations: keys.iter().map(|key| self.get1(key, None, now)).collect(),
        }
    }

    /// close a get operation for a given `handle`
    #[instrument(level = "debug", skip(self), err, fields(use_count))]
    pub fn close1_deprecated(&self, key: &Arc<[u8]>) -> Result<()> {
        match self.open_records.entry(key.clone()) {
            Entry::Occupied(mut entry) => {
                match entry.get_mut() {
                    OpenRecord::Get { use_count, .. } => {
                        tracing::Span::current().record("use_count", *use_count);
                        assert!(*use_count > 0);

                        // remove from map if no longer in use by anyone
                        if *use_count == 1 {
                            entry.remove();
                        } else {
                            *use_count -= 1;
                        }

                        Ok(())
                    },
                    OpenRecord::Put { .. } => {
                        error!(key = %HexSlice::new(entry.key()), "key not open");
                        Err(Error::KeyNotOpen(format!("{}", HexSlice::new(entry.key()))))
                    },
                    OpenRecord::Abandoned { .. } => {
                        error!(key = %HexSlice::new(entry.key()), "key not open");
                        Err(Error::KeyNotOpen(format!("{}", HexSlice::new(entry.key()))))
                    },
                }
            },
            Entry::Vacant(entry) => {
                error!(key = %HexSlice::new(entry.key()), "key not open");
                Err(Error::KeyNotOpen(format!("{}", HexSlice::new(entry.key()))))
            },
        }
    }

    /// close get operations for a slice of keys, returning the results as a vector.
    #[instrument(level = "debug", skip_all, fields(len = keys.len()))]
    pub fn block_close_deprecated(&self, keys: &[Arc<[u8]>]) -> Vec<Result<()>> {
        keys.iter().map(|key| self.close1_deprecated(key)).collect()
    }

    /// get a file based key
    #[instrument(level = "debug", skip_all, ret, fields(now))]
    pub fn file_get(&self, key: &Arc<[u8]>, extension: &Arc<str>) -> Result<Option<PathBuf>> {
        let now = self.now.get();
        record!(now);

        self.get1(key, Some(extension.clone()), now).map(|result| {
            result.map(|location| {
                // TODO check
                //   location.offset == 0
                //   could also add check in get1() that flags & FILE
                //   check file exists on debug build?
                self.file_pool
                    .base_path()
                    .join(location.path.with_extension(extension.as_ref()))
            })
        })
    }

    /// put a file based key
    #[instrument(level = "debug", skip(self), ret, fields(now))]
    pub fn file_put(&self, request: &PutRequest, client_id: &Uuid) -> Result<PathBuf> {
        let now = self.now.get();
        record!(now);

        // extensions are required for file based caching
        let Some(extension) = &request.extension else {
            return Err(Error::FileMissingExtension(format!("{}", HexSlice::new(&request.key))));
        };

        let result = self.put1(request, now, client_id).map(|location| {
            assert_eq!(location.offset, 0);
            self.file_pool
                .base_path()
                .join(location.path.with_extension(extension.as_ref()))
        });
        self.flush()?;
        result
    }

    /// close a file based key
    #[instrument(level = "debug", skip_all, err, fields(now, size, use_count))]
    pub fn file_close_deprecated(&self, key: &Arc<[u8]>) -> Result<()> {
        let now = self.now.get();
        record!(now);

        match self.open_records.entry(key.clone()) {
            Entry::Occupied(mut entry) => match entry.get_mut() {
                OpenRecord::Get { use_count, .. } => {
                    tracing::Span::current().record("use_count", *use_count);

                    if use_count == &1 {
                        entry.remove();
                    } else {
                        *use_count -= 1;
                    }

                    Ok(())
                },
                OpenRecord::Put { value, extension } => {
                    let mut record = *bytemuck::from_bytes::<RecordV1>(value);

                    // use fs::metadata() to get final size, update record, flush
                    let path = self
                        .file_pool
                        .base_path()
                        .join(FilePool::file_path(record.data_file, extension));
                    let size = fs::metadata(path)?.len();
                    record!(size);

                    record.last_accessed = now;
                    record.flags.insert(FlagsV1::COMMITTED);
                    record.data_size = size;

                    let previous = self.index.insert(entry.key(), record)?;
                    assert!(previous.is_some());

                    self.flush()?;

                    // record increased cache size
                    self.entries.fetch_add(1, Ordering::Relaxed);
                    self.size.fetch_add(record.data_size, Ordering::Relaxed);

                    // stats on total data stored in cache
                    self.stats.data_stored(record.data_size);

                    // index record now sorted, we can remove from open records map
                    entry.remove();

                    self.flush()?;
                    Ok(())
                },
                OpenRecord::Abandoned { .. } => {
                    warn!(key = %HexSlice::new(entry.key()), "file close() on abandoned key!");
                    Err(Error::KeyNotFound(format!("{}", HexSlice::new(entry.key()))))
                },
            },
            Entry::Vacant(entry) => {
                warn!(key = %HexSlice::new(entry.key()), "file close() on unknown key!");
                Err(Error::KeyNotFound(format!("{}", HexSlice::new(entry.key()))))
            },
        }
    }

    /// remove a file based key
    ///   -- it is possible to optimize disk space by removing the file immediately here
    ///      and not waiting for garbage collection...
    #[instrument(level = "debug", skip_all, err, fields(use_count))]
    pub fn file_remove_deprecated(&self, key: &Arc<[u8]>) -> Result<()> {
        match self.open_records.entry(key.clone()) {
            Entry::Occupied(mut entry) => match entry.get_mut() {
                OpenRecord::Get { use_count, .. } => {
                    tracing::Span::current().record("use_count", *use_count);
                    // only remove if last reference
                    if *use_count == 1 {
                        debug!(key = %HexSlice::new(entry.key()), "removing get");
                        self.index.remove(entry.key())?;
                        entry.remove();
                        self.flush()?;
                    } else {
                        *use_count -= 1;
                    }
                },
                OpenRecord::Put { .. } => {
                    debug!(key = %HexSlice::new(entry.key()), "removing put");
                    self.index.remove(entry.key())?;
                    entry.remove();
                    self.flush()?;
                },
                OpenRecord::Abandoned { .. } => {
                    debug!(key = %HexSlice::new(entry.key()), "removing abandoned");
                    self.index.remove(entry.key())?;
                    entry.remove();
                    self.flush()?;
                },
            },
            Entry::Vacant(_) => {
                // file is not open, just remove it from database
                self.index.remove(key)?;
                self.flush()?;
            },
        }

        Ok(())
    }

    /// abandon a file put operation
    #[instrument(level = "debug", skip_all, err)]
    pub fn file_abandon(&self, key: &Arc<[u8]>) -> Result<()> {
        self.abandon1(key)
    }

    /// attempt to clear the database. it will only remove items from index that have use_count == 0
    /// data in files is not cleared or recovered until garbage collection is run
    /// returns the number of entries removed
    #[instrument(level = "info", skip_all, ret)]
    pub async fn clear(&self) -> Result<u64> {
        let mut removed = 0;
        let mut removed_size = 0;

        for (key, _) in self.index.iter().filter_map(|e| e.ok()) {
            let key: Arc<[u8]> = key.as_ref().into();

            match self.open_records.entry(key) {
                Entry::Occupied(entry) => {
                    // key in open status, cannot remove
                    trace!(key = ?entry.key(), open_record = ?entry.get());
                },
                Entry::Vacant(entry) => {
                    // key not in use, can be removed
                    // we need to use entry() and not get(), because if the key
                    // is not found, we still want to hold the entry locked while we
                    // remove it from the index...
                    if let Some(value) = self.index.remove(entry.key())? {
                        let record = bytemuck::from_bytes::<RecordV1>(&value);
                        trace!(key = ?entry.key(), %record, "removed");
                        removed += 1;
                        removed_size += record.data_size;
                    }
                },
            }
        }

        // adjust cached size
        if removed > 0 {
            self.entries.fetch_sub(removed, Ordering::Relaxed);
            self.size.fetch_sub(removed_size, Ordering::Relaxed);
        }

        // flush results, while not strictly necessary for recovery, this is already
        // a pretty heavy operation, flushing page cache anyways
        self.flush()?;

        // trigger garage collection to clean up any data files
        let bytes_collected = self.gc().await?;
        trace!(bytes_collected, "garbage collected");

        Ok(removed)
    }

    /// flush index to disk
    #[instrument(level = "debug", skip_all, err, fields(bytes))]
    pub fn flush(&self) -> Result<usize> {
        let bytes = self.index.flush()?;
        record!(bytes);
        Ok(bytes)
    }

    /// fetch the value, apply a function to it and return the result.
    /// not using the built in sled update_and_fetch due to lack of Result support.
    /// and convenience of conversion from IVec to RecordV1.
    ///
    /// `f` maybe called multiple times if value is being changed by other threads
    /// and CAS operation fails.
    fn update_and_fetch<F>(&self, key: &[u8], mut f: F) -> Result<Option<RecordV1>>
    where
        F: FnMut(&RecordV1) -> Result<Option<RecordV1>>,
    {
        let mut current = self.index.get(key)?;

        loop {
            match current.as_ref().map(AsRef::as_ref) {
                Some(old) => {
                    // cast bytes to RecordV1
                    let record = bytemuck::from_bytes::<RecordV1>(old);

                    // call function update old and return new value
                    let updated_record = f(record)?;

                    match self.index.compare_and_swap(key, Some(old), updated_record)? {
                        Ok(()) => {
                            break Ok(updated_record);
                        },
                        Err(sled::CompareAndSwapError { current: cur, .. }) => {
                            // CAS failed, reset current and retry
                            current = cur;
                            continue;
                        },
                    }
                },
                None => {
                    // key not found, do not update anything
                    break Err(Error::KeyNotFound(format!("{}", HexSlice::new(key))));
                },
            }
        }
    }

    /// garbage collection task
    #[instrument(level = "info", err, skip_all)]
    async fn gc_task(&self) -> Result<()> {
        info!("Starting garage collection task");

        loop {
            // https://docs.rs/tokio/latest/tokio/time/struct.Sleep.html
            // sleep does not implement Unpin, must pin it to use with select
            let sleep = sleep(self.garbage_collection_interval.get());
            tokio::pin!(sleep);

            tokio::select! {
                () = &mut sleep => {
                    info!("starting garbage collection");

                }
                () = self.garbage_collection_notify.notified() => {
                    info!("starting garbage collection: triggered")
                }
            }

            let collected = self.gc().await?;
            info!(collected, "garbage collection finished");
        }
    }

    // garbage collection, returns number of bytes collected (from index)
    #[instrument(level = "info", skip(self), ret, fields(collected))]
    pub async fn gc(&self) -> Result<u64> {
        // only allow 1 gc operation concurrently
        let _permit = self.garbage_collection_lock.acquire().await?;

        // even if we are not going to evict items (cache size is smaller than max),
        // we still want to check for orphan data files, maybe from a clear operation,
        // check for compaction opportunities, etc

        // collect all the files in the active cache directory
        // important to collect these before iterating the index
        let (mut data_files, unknown_files) = self.file_pool.files()?;

        // mark all entries in database that are not in use, sorting by rank
        // only do this, if we are over cache size

        let mut collected = 0;

        if self.size.load(Ordering::Relaxed) > self.max_size.get() {
            // TODO: move this to a separate sled db/tree
            let mut rankings: BTreeMap<u64, HashSet<sled::IVec>> = BTreeMap::new();

            // mark phase
            for entry in self.index.iter() {
                let (key_ivec, value) = entry?;
                let record = bytemuck::from_bytes::<RecordV1>(value.as_ref());
                trace!(%record);

                // optimistically do not include anything that is in use (probably low ranking anyways)
                let key: Arc<[u8]> = Arc::from(key_ivec.as_ref());

                if !self.open_records.contains_key(&key) {
                    use std::collections::btree_map::Entry;

                    // TODO: finalize ranking
                    let rank = record.last_accessed;
                    trace!(key = %HexSlice::new(&key), rank, "mark");

                    // for duplicate rankings, we just store a hash set of items (pseudo random order for matched rankings)
                    match rankings.entry(rank) {
                        Entry::Occupied(mut e) => {
                            let inserted = e.get_mut().insert(key_ivec);
                            assert!(inserted); // this should never fail otherwise sled had duplicate key
                        },
                        Entry::Vacant(e) => {
                            let mut set = HashSet::new();
                            set.insert(key_ivec);
                            e.insert(set);
                        },
                    }
                }
            }

            let new_size = self.max_size.get() - (self.max_size.get() * self.garbage_collection_threshold.get() / 100);
            trace!(current_size = self.size.load(Ordering::Relaxed), new_size);

            // sweep phase - clean up until threshold in order to prevent thrashing
            // keys are sorted by ranking, keys with same rank are chosen at random via hash set

            for key in rankings.iter().flat_map(|e| e.1) {
                // check to see if we managed to go under new size
                if self.size.load(Ordering::Relaxed) <= new_size {
                    break;
                }

                let key: Arc<[u8]> = Arc::from(key.as_ref());
                match self.open_records.entry(key) {
                    Entry::Occupied(mut entry) => {
                        if let OpenRecord::Abandoned { value: _, gc_intervals } = entry.get_mut() {
                            *gc_intervals += 1;

                            // after 3 intervals, remove this abandoned record, it can
                            // always be put back into the cache
                            if gc_intervals == &3 {
                                if let Some(value) = self.index.remove(entry.key().as_ref())? {
                                    let record = bytemuck::from_bytes::<RecordV1>(value.as_ref());
                                    trace!(key = ?entry.key().as_ref(), %record, "collecting abandoned record");

                                    // decrement cache counters
                                    self.entries.fetch_sub(1, Ordering::Relaxed);
                                    self.size.fetch_sub(record.data_size, Ordering::Relaxed);
                                    collected += record.data_size;
                                }
                            }
                        }
                    },
                    Entry::Vacant(entry) => {
                        if let Some(value) = self.index.remove(entry.key().as_ref())? {
                            let record = bytemuck::from_bytes::<RecordV1>(value.as_ref());
                            trace!(key = ?entry.key().as_ref(), %record, "collecting");

                            // decrement cache counters
                            self.entries.fetch_sub(1, Ordering::Relaxed);
                            self.size.fetch_sub(record.data_size, Ordering::Relaxed);
                            collected += record.data_size;
                        }
                    },
                }
            }
        }

        record!(collected);

        // intersect files in cache dir with files in index
        // purge any files that have no links

        for entry in self.index.iter() {
            let (key, value) = entry?;
            let record = bytemuck::from_bytes::<RecordV1>(value.as_ref());
            let data_file = data_files.remove(&record.data_file);
            info!(key = %HexSlice::new(&key), %record, ?data_file, "in-use");
        }

        // remove any data files not found in index
        for (id, (size, path)) in data_files {
            info!(id, size, ?path, "removing unused data file");
            self.file_pool.remove_file(id, size, path)?;
        }

        // remove unknown files
        for path in unknown_files {
            warn!(?path, "removing unknown file");
            remove_file(path)?;
        }

        Ok(collected)
    }

    /// update LRU time for a given key
    #[instrument(level = "info", skip_all, ret)]
    fn lru_update(&self, key: &Arc<[u8]>, now: &u64) -> Result<()> {
        match self
            .index
            .get(key)?
            .as_ref()
            .map(AsRef::as_ref)
            .map(bytemuck::from_bytes::<RecordV1>)
            // only fully committed records, ie not in the middle of a put()
            .filter(|record| record.flags.contains(FlagsV1::COMMITTED))
        {
            Some(current) => {
                let mut record = *current;
                record.last_accessed = *now;
                record.accesses += 1;

                if let Err(sled::CompareAndSwapError { current, proposed }) =
                    self.index
                        .compare_and_swap(key, Some(bytemuck::bytes_of(current)), Some(record))?
                {
                    let current_record = current.as_ref().map(|v| bytemuck::from_bytes::<RecordV1>(v));
                    let proposed_record = proposed.as_ref().map(|v| bytemuck::from_bytes::<RecordV1>(v));
                    warn!(key = %HexSlice::new(&key), now, ?current_record, ?proposed_record, "Unable to update LRU record!");
                }
            },
            None => {
                warn!(key = %HexSlice::new(&key), now, "LRU update on unknown key!");
            },
        }

        Ok(())
    }

    /// update records last accessed, accesses in the background in order to try and keep
    /// any database writes out of the fast path -> get()/stat()
    #[instrument(level = "info", skip_all, ret)]
    async fn lru_task(&self) -> Result<()> {
        info!("Starting LRU task");

        // lru channel should be initialized via run()
        let lru_channel = self
            .lru_channel
            .get()
            .ok_or(Error::InternalError("Unable to get LRU channel".to_owned()))?;

        // only 1 lru task should be be running, if try_lock fails, means another task already
        // handling the updates
        let mut rx = lru_channel.rx.try_lock()?;

        let limit = 32;
        let mut buffer = Vec::with_capacity(limit);

        while rx.recv_many(&mut buffer, limit).await > 0 {
            for (key, now) in &buffer {
                trace!(key = %HexSlice::new(&key), now, "LRU update");

                // not performing any open records locking, etc
                // this is a optimisitic LRU update, if the record exists
                // and the update is good, then great, otherwise we just move on...
                // the record could have been cleared or removed by the time the LRU
                // update request is processed

                if let Err(e) = self.lru_update(key, now) {
                    warn!(key = %HexSlice::new(&key), now, ?e, "Unable to update LRU record!");
                }
            }
        }

        Ok(())
    }

    /// update cached now at second granularity.. anything else is just overhead
    /// also keeps expensive clock/syscall from fast paths like get()
    #[instrument(level = "info", skip_all, ret)]
    async fn clock_task(&self) -> Result<()> {
        info!("Starting clock update task");
        loop {
            self.clock_update()?;
            sleep(Duration::from_secs(1)).await;
        }
    }

    fn clock_update(&self) -> Result<()> {
        self.now.set(Self::now()?);
        Ok(())
    }

    /// run cache background operations.
    ///   - garbage collection
    ///   - LRU
    ///   - file pool
    #[instrument(level = "info", skip_all, err)]
    pub fn run(&self) -> Result<JoinSet<Result<()>>> {
        let mut tasks = JoinSet::new();

        // clock updates
        tasks.spawn({
            let cache = self.clone();
            async move { cache.clock_task().await }
        });

        // background LRU updates
        tasks.spawn({
            let cache = self.clone();

            // we setup the mpsc channel, if this task is not running, the oncecell will be empty
            // and senders will get None back and skip the LRU updates
            let (tx, rx) = mpsc::unbounded_channel();
            let lru_channel = LRUChannel { tx, rx: Mutex::new(rx) };

            self.lru_channel
                .set(lru_channel)
                .map_err(|e| Error::InternalError(format!("Unable to set LRU channel: {e:?}")))?;

            async move { cache.lru_task().await }
        });

        // garbage collection
        tasks.spawn({
            let cache = self.clone();
            async move { cache.gc_task().await }
        });

        Ok(tasks)
    }

    /// get cache stats
    #[instrument(level = "debug", skip_all, ret)]
    pub fn stats(&self) -> Stats {
        self.stats.stats(
            self.entries.load(Ordering::Relaxed),
            self.size.load(Ordering::Relaxed),
            self.file_pool.size_on_disk(),
            self.db.size_on_disk().unwrap_or(0),
            self.started.elapsed().as_secs(),
        )
    }

    /// reset current cache stats
    #[instrument(level = "debug", skip_all)]
    pub fn clear_stats(&self) {
        self.stats.clear();
    }

    /// dump database to file
    pub async fn dump_database(&self) -> Result<()> {
        for (key, val) in self.index.iter().filter_map(|e| e.ok()) {
            let key: Arc<[u8]> = key.as_ref().into();
            let record = bytemuck::from_bytes::<RecordV1>(val.as_ref());

            match self.open_records.entry(key) {
                Entry::Occupied(entry) => match entry.get() {
                    OpenRecord::Get { use_count, location } => {
                        trace!(key = %HexSlice::new(entry.key()), %record, use_count, ?location, "get");
                    },
                    OpenRecord::Put { .. } => {
                        trace!(key = %HexSlice::new(entry.key()), %record, "put");
                    },
                    OpenRecord::Abandoned { .. } => {
                        trace!(key = %HexSlice::new(entry.key()), %record, "abandoned");
                    },
                },
                Entry::Vacant(entry) => {
                    trace!(key = %HexSlice::new(entry.key()), %record, "key not open");
                },
            }
        }

        for open_record in &self.open_records {
            trace!(key = %HexSlice::new(open_record.key()), open_record = ?open_record.value());
        }

        info!(len = self.index.len(), open = self.open_records.len());

        // TODO stream database to file? or possibly stream it directly into zip file writer?
        // use struson::writer::{JsonStreamWriter, JsonWriter};

        // let mut file = std::fs::OpenOptions::new()
        //     .create(true)
        //     .truncate(true)
        //     .write(true)
        //     .open(path)?;
        // let mut json_writer = JsonStreamWriter::new(&mut file);

        // let uuid: Uuid = match self.system.get("uuid")? {
        //     Some(entry) => bincode::deserialize(entry.as_ref())?,
        //     None => Uuid::new_v4(),
        // };

        // let version: u64 = match self.system.get("version")? {
        //     Some(entry) => bincode::deserialize(entry.as_ref())?,
        //     None => 0,
        // };

        // let system = serde_json::json!({ "uuid": uuid, "version": version });

        // json_writer.begin_object()?;
        // json_writer.name("system")?;
        // json_writer.serialize_value(&system)?;
        // json_writer.end_object()?;
        // json_writer.finish_document()?;

        Ok(())
    }
}

impl fmt::Display for Cache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Cache [ uuid={}, version={:?}, path={} ]",
            self.uuid,
            self.version,
            self.path.display(),
        )
    }
}

/// wrapper for LRU update channels
/// channel data is (key, time in seconds) => (Bytes, u64)
#[derive(Debug)]
struct LRUChannel {
    tx: mpsc::UnboundedSender<(Arc<[u8]>, u64)>,
    rx: Mutex<mpsc::UnboundedReceiver<(Arc<[u8]>, u64)>>,
}

/// struct for tracking cache stats internally
#[derive(Debug)]
struct InternalStats {
    /// total number of cache hits since startup
    cache_hits: AtomicU64,
    /// total number of cache accesses (hits and misses) since startup
    cache_accesses: AtomicU64,
    /// total amount of data stored in bytes over time since startup
    total_stored_data: AtomicU64,
    /// total times get is called for a key while a put is still outstanding
    gets_during_put: AtomicU64,
    /// total times put is called for a key while another put is still outstanding
    puts_during_put: AtomicU64,
}

impl InternalStats {
    fn new() -> Self {
        Self {
            cache_hits: AtomicU64::new(0),
            cache_accesses: AtomicU64::new(0),
            total_stored_data: AtomicU64::new(0),
            gets_during_put: AtomicU64::new(0),
            puts_during_put: AtomicU64::new(0),
        }
    }

    fn clear(&self) {
        self.cache_hits.store(0, Ordering::Relaxed);
        self.cache_accesses.store(0, Ordering::Relaxed);
        self.gets_during_put.store(0, Ordering::Relaxed);
        self.puts_during_put.store(0, Ordering::Relaxed);
    }

    fn hit(&self) {
        self.cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    fn access(&self) {
        self.cache_accesses.fetch_add(1, Ordering::Relaxed);
    }

    fn data_stored(&self, size: u64) {
        self.total_stored_data.fetch_add(size, Ordering::Relaxed);
    }

    fn get_during_put(&self) {
        self.gets_during_put.fetch_add(1, Ordering::Relaxed);
    }

    fn put_during_put(&self) {
        self.puts_during_put.fetch_add(1, Ordering::Relaxed);
    }

    fn stats(
        &self,
        cache_entries: u64,
        cache_size: u64,
        cache_on_disk_size: u64,
        database_size: u64,
        uptime: u64,
    ) -> Stats {
        Stats {
            cache_entries,
            cache_size,
            cache_hits: self.cache_hits.load(Ordering::Relaxed),
            cache_misses: self.cache_accesses.load(Ordering::Relaxed) - self.cache_hits.load(Ordering::Relaxed),
            total_stored_data: self.total_stored_data.load(Ordering::Relaxed),
            reads_during_file_write: self.gets_during_put.load(Ordering::Relaxed),
            writes_during_file_write: self.puts_during_put.load(Ordering::Relaxed),
            cache_on_disk_size,
            database_size,
            uptime,
        }
    }
}

/// statistics about the cache
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Stats {
    /// The current number of files stored in the cache
    pub cache_entries: u64,
    /// The current size of all files in the cache
    pub cache_size: u64,
    /// total number of cache hits since hub was started
    pub cache_hits: u64,
    /// total number of cache misses since hub was started
    pub cache_misses: u64,
    /// The total amount of data that has been stored in the cache, in bytes
    pub total_stored_data: u64,
    /// The number of times that a file was requested for read while it was being written
    pub reads_during_file_write: u64,
    /// The number of times that a file was requested for write while it was being written
    pub writes_during_file_write: u64,
    /// The current size of all files in the cache including files not yet garbage collected
    pub cache_on_disk_size: u64,
    /// size of the database on disk
    pub database_size: u64,
    /// uptime of cache in seconds
    pub uptime: u64,
}

#[derive(Debug, Clone)]
pub struct Location {
    /// relative path to file (relative to cache directory)
    pub path: PathBuf,
    /// id of data file
    pub id: u64,
    /// offset in bytes to data within data file
    pub offset: u64,
    /// size in bytes of data
    pub size: u64,
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Location {{ id: {}, offset: {}, size: {} }}",
            self.id, self.offset, self.size
        )
    }
}

#[derive(Debug, Clone)]
pub struct PutRequest {
    /// key to put into cache
    pub key: Arc<[u8]>,
    /// size of key's data in bytes to put into cache
    pub size: u64,
    /// Garbage collection rank
    pub rank: u64,
    /// storage data in standalone / unique file with extension
    pub extension: Option<Arc<str>>,
}

impl PutRequest {
    /// create a put request from `key`, `size`, `rank`, and `extension`
    pub fn new(key: impl AsRef<[u8]>, size: u64, rank: u64, extension: Option<Arc<str>>) -> Self {
        Self {
            key: Arc::from(key.as_ref()),
            size,
            rank,
            extension,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConnectRequest {
    /// Key to reopen
    pub key: Arc<[u8]>,
    /// File extension
    pub extension: Option<Arc<str>>,
    /// True if the key is currently being put
    pub open_for_put: bool,
}

#[derive(Debug)]
pub struct PutResult {
    /// base directory to which all handles reside in
    pub base: PathBuf,
    /// vector of results, containing the handles to each successful put operation start.
    /// these handles either need to be committed or aborted once the client has copied data
    pub locations: Vec<Result<Location>>,
}

#[derive(Debug)]
pub struct GetResult {
    /// base directory to which all handles reside in
    pub base: PathBuf,
    /// vector of results, containing the location to each successful get operation.
    /// these locations need to be closed() to indicate they are no longer in-use
    /// `None` indicates that particular key was not found
    pub locations: Vec<Result<Option<Location>>>,
}

#[derive(Debug, Clone)]
pub struct Metadata {
    /// seconds since EPOCH of last access
    pub last_accessed: u64,
    /// number of times get() or stat() was called on key
    pub accesses: u64,
    /// size in bytes
    pub size: u64,
}

/// Index Record v1, alignment 8
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
#[repr(C)]
#[allow(dead_code)]
struct RecordV1 {
    /// bit flags for record @see `FlagsV1`
    flags: FlagsV1,

    /// data file id
    data_file: u64,
    /// offset in bytes in data file
    data_offset: u64,
    /// size in bytes of data
    data_size: u64,

    /// time in seconds since epoch since last accessed
    last_accessed: u64,
    /// total number of times this record was accessed
    accesses: u64,
    /// garage collection ranking. set by client
    rank: u64,

    /// UUIDv4 client identifer
    client_uuid: [u8; 16],

    /// reserved field (64-bit unsigned integer for debugging)
    reserved0: u64,
    /// reserved field (32 bytes to allow 256-bit hash for debugging)
    reserved1: [u8; 32],
}

impl fmt::Display for RecordV1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "RecordV1 {{ flags={:?}, last_accessed={}, accesses={}, rank={}, file={:016X}, offset={}, size={} }}",
            self.flags, self.last_accessed, self.accesses, self.rank, self.data_file, self.data_offset, self.data_size,
        )
    }
}

impl From<RecordV1> for sled::IVec {
    fn from(value: RecordV1) -> Self {
        bytemuck::bytes_of(&value).into()
    }
}

impl From<&RecordV1> for sled::IVec {
    fn from(value: &RecordV1) -> Self {
        bytemuck::bytes_of(value).into()
    }
}

impl From<&mut RecordV1> for sled::IVec {
    fn from(value: &mut RecordV1) -> Self {
        bytemuck::bytes_of(value).into()
    }
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
    #[repr(transparent)]
    pub struct FlagsV1: u64 {
        /// record has been committed to cache
        const COMMITTED = 0x1;
        /// record is file based, not block based
        const FILE = 0x1 << 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::prelude::*;
    use std::ops::DerefMut;

    use assert_matches::assert_matches;
    use positioned_io::{ReadAt, WriteAt};
    use rstest::{fixture, rstest};
    use tempfile::{tempdir, TempDir};

    // db test fixture, using tmp directory for each test case
    #[derive(Debug)]
    struct CacheFixture {
        _tmp_dir: Option<TempDir>,
        cache: Cache,
        background_tasks: Option<JoinSet<Result<()>>>,
        client_id: Uuid,
        data_file_size: ByteSize,
    }

    impl Default for CacheFixture {
        fn default() -> Self {
            let tmp_dir = tempdir().unwrap();
            let data_file_size = ByteSize::kib(512);
            let cache = CacheBuilder::new(tmp_dir.path())
                .page_cache_capacity(ByteSize::mb(16))
                .max_size(ByteSize::mib(4))
                .unwrap()
                .data_file_size(data_file_size)
                .open()
                .unwrap();

            Self {
                _tmp_dir: Some(tmp_dir),
                cache,
                background_tasks: None,
                client_id: Uuid::new_v4(),
                data_file_size,
            }
        }
    }

    impl Deref for CacheFixture {
        type Target = Cache;

        fn deref(&self) -> &Self::Target {
            &self.cache
        }
    }

    impl DerefMut for CacheFixture {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.cache
        }
    }

    impl CacheFixture {
        fn new(path: impl AsRef<Path>) -> Self {
            let data_file_size = ByteSize::kib(32);
            let cache = CacheBuilder::new(path)
                .page_cache_capacity(ByteSize::mb(16))
                .max_size(ByteSize::mib(1))
                .unwrap()
                .data_file_size(data_file_size)
                .open()
                .unwrap();

            Self {
                _tmp_dir: None,
                cache,
                background_tasks: None,
                client_id: Uuid::new_v4(),
                data_file_size,
            }
        }

        fn run_tasks(&mut self) -> Result<()> {
            self.background_tasks = Some(self.cache.run()?);
            Ok(())
        }
    }

    #[fixture]
    fn cache() -> CacheFixture {
        CacheFixture::default()
    }

    #[rstest]
    #[test_log::test(tokio::test)]
    async fn system() {
        // TODO: system table in sled would hold, UUID, version, like the original file cache
        // write some tests verifying those operations
    }

    #[rstest]
    #[test_log::test(tokio::test)]
    async fn empty(cache: CacheFixture) {
        assert!(cache.index.is_empty());
        assert_eq!(cache.size.load(Ordering::Relaxed), 0);
    }

    #[rstest]
    #[test_log::test(tokio::test)]
    async fn block_stat1(mut cache: CacheFixture) {
        // run background tasks for clock and lru updates
        cache.run_tasks().unwrap();

        // invalid argument / key not found stat calls
        assert_matches!(cache.stat1(&b""[..].into(), 0).unwrap_err(), Error::KeyEmpty);
        assert!(cache.stat1(&"key".as_bytes().into(), 0).unwrap().is_none());

        // place key/value into cache (don't really need to write any data to disk for stat test)
        let data1 = b"world";
        let request1 = PutRequest::new("hello", data1.len() as u64, 0, None);
        let location1 = cache.put1(&request1, 0, &cache.client_id).unwrap();
        assert_eq!(location1.size, data1.len() as u64);
        // set now to 0, so we are sure stat1() call queued and updated the LRU on the background task
        let result = cache.commit1_deprecated(&b"hello"[..].into(), 0);
        assert!(result.is_ok());

        // test stat
        let now = cache.now.get();
        let result1 = cache.stat1(&b"hello"[..].into(), now);
        assert!(result1.is_ok());
        let metadata1 = result1.unwrap().unwrap();
        assert_eq!(metadata1.size, data1.len() as u64);

        trace!(now, ?metadata1);

        // these fields are updated on the LRU task
        // so we need to retry to be sure we see the updated values
        let mut last_accessed = 0;
        let mut accesses = 0;
        for _ in 0..5 {
            let entry = cache.index.get("hello").unwrap().unwrap();
            let record = bytemuck::from_bytes::<RecordV1>(entry.as_ref());
            if record.last_accessed == now {
                last_accessed = record.last_accessed;
                accesses = record.accesses;
                break;
            }
            sleep(Duration::from_secs(1)).await;
        }
        assert_eq!(last_accessed, now);
        assert_eq!(accesses, 1);
    }

    #[rstest]
    #[test_log::test(tokio::test)]
    async fn block_stat(cache: CacheFixture) {
        // invalid argument / key not found stat calls
        let keys: Vec<Arc<[u8]>> = vec![b""[..].into(), b"key"[..].into()];
        let result = cache.block_stat(&keys);
        assert_eq!(result.len(), keys.len());
        assert_matches!(result[0].as_ref().unwrap_err(), Error::KeyEmpty);
        assert!(result[1].as_ref().unwrap().is_none());

        // place key/value into cache (don't really need to write any data to disk for stat test)
        let data1 = b"world";
        let requests = vec![PutRequest::new("hello", data1.len() as u64, 0, None)];
        let result1 = cache.block_put(&requests, &cache.client_id).unwrap();
        assert_eq!(result1.locations.len(), requests.len());
        assert!(result1.locations[0].is_ok());

        // grab all the ok results, which should be all of them
        let locations1: Vec<_> = result1.locations.into_iter().filter_map(Result::ok).collect();
        assert_eq!(locations1.len(), requests.len());
        let result1 = cache.block_commit_deprecated(&[b"hello"[..].into()]).unwrap();
        assert_eq!(result1.len(), locations1.len());
        let result1 = result1.into_iter().collect::<Result<()>>();
        assert!(result1.is_ok());

        // test stat
        let keys: Vec<Arc<[u8]>> = vec![b"hello"[..].into(), b"key-not-found"[..].into()];
        let result1 = cache.block_stat(&keys);
        assert_eq!(result1.len(), keys.len());
        info!(?result1);
        assert!(result1[0].is_ok());
        assert!(result1[0].as_ref().unwrap().is_some());
        assert!(result1[1].is_ok());
        assert!(result1[1].as_ref().unwrap().is_none());

        // unwrap the results, all the keys should unwrap, key-not-found will be None, not errors
        let metadata1: Vec<_> = result1.into_iter().map(Result::unwrap).collect();
        assert_eq!(metadata1.len(), keys.len());
        assert_eq!(metadata1[0].as_ref().unwrap().size, data1.len() as u64);
        assert!(metadata1[1].is_none());
    }

    #[rstest]
    #[test_log::test(tokio::test)]
    async fn block_put1(cache: CacheFixture) {
        // invalid arguments
        assert_matches!(
            cache
                .put1(&PutRequest::new("", 0, 0, None), 0, &cache.client_id)
                .unwrap_err(),
            Error::KeyEmpty
        );
        assert_matches!(
            cache
                .put1(&PutRequest::new("", 66, 0, None), 0, &cache.client_id)
                .unwrap_err(),
            Error::KeyEmpty
        );
        assert_matches!(
            cache
                .put1(&PutRequest::new("a-b-c/0/1/2/?", 0, 0, None), 0, &cache.client_id)
                .unwrap_err(),
            Error::InvalidArgument(_)
        );

        // put data
        let data1 = b"hello, world";
        let location1 = cache
            .put1(&PutRequest::new("k1", data1.len() as u64, 0, None), 0, &cache.client_id)
            .unwrap();
        assert_eq!(location1.offset, 0);
        assert_eq!(location1.size, data1.len() as u64);

        // put data
        let data2 = b"goodbye";
        let location2 = cache
            .put1(&PutRequest::new("k2", data2.len() as u64, 0, None), 0, &cache.client_id)
            .unwrap();
        assert_eq!(location2.offset, data1.len() as u64);
        assert_eq!(location2.size, data2.len() as u64);

        // not committed yet
        assert!(cache.stat1(&b"k1"[..].into(), 0).unwrap().is_none());
        assert!(cache.stat1(&b"k2"[..].into(), 0).unwrap().is_none());

        // commit/abort puts
        cache.commit1_deprecated(&b"k1"[..].into(), 0).unwrap();
        cache.abort1_deprecated(&b"k2"[..].into()).unwrap();

        // k1 committed, metadata should be present
        let metadata1 = cache.stat1(&b"k1"[..].into(), 0).unwrap().unwrap();
        assert_eq!(metadata1.size, data1.len() as u64);

        // k2 should be missing, put was aborted
        assert!(cache.stat1(&b"k2"[..].into(), 0).unwrap().is_none());
    }

    #[rstest]
    #[test_log::test(tokio::test)]
    async fn block_put_invalid(cache: CacheFixture) {
        let requests = vec![PutRequest::new("", 0, 0, None), PutRequest::new("", 10, 0, None)];

        cache
            .block_put(&requests, &cache.client_id)
            .unwrap()
            .locations
            .iter()
            .for_each(|result| {
                assert_matches!(result.as_ref().unwrap_err(), Error::KeyEmpty);
            });

        let requests = vec![PutRequest::new("key", 0, 0, None)];

        cache
            .block_put(&requests, &cache.client_id)
            .unwrap()
            .locations
            .iter()
            .for_each(|result| {
                assert_matches!(result.as_ref().unwrap_err(), Error::InvalidArgument(_));
            });
    }

    #[rstest]
    #[test_log::test(tokio::test)]
    async fn block_put_abort(cache: CacheFixture) {
        let data: Vec<Arc<str>> = vec![
            Arc::from("foo"),
            Arc::from("hello, world"),
            Arc::from("0123456789-abc123!@#$%^"),
        ];

        let requests = vec![
            PutRequest::new("k1", data[0].len() as u64, 0, None),
            PutRequest::new("foo-bar", data[1].len() as u64, 0, None),
            PutRequest::new("key", data[2].len() as u64, 0, None),
        ];

        // attempt to put 3 items
        let result = cache.block_put(&requests, &cache.client_id).unwrap();

        // test the base path and that we get the same number of results as requests
        assert_eq!(result.base, cache.file_pool.base_path());
        assert_eq!(result.locations.len(), requests.len());

        // validate each result
        for (i, location) in result
            .locations
            .iter()
            .map(|location| location.as_ref().unwrap())
            .enumerate()
        {
            assert_eq!(location.size, data[i].len() as u64);
            assert_eq!(location.path, FilePool::file_path(location.id, &None));
        }

        // abort all operations
        let locations: Vec<_> = result.locations.into_iter().filter_map(Result::ok).collect();
        assert_eq!(locations.len(), requests.len());
        let result = cache
            .block_abort_deprecated(&[b"k1"[..].into(), b"foo-bar"[..].into(), b"key"[..].into()])
            .unwrap();
        assert_eq!(result.len(), requests.len());
        let result: Result<()> = result.into_iter().collect();
        assert!(result.is_ok());

        // aborted all operations, cache should be empty
        assert!(cache.index.is_empty());
        assert_eq!(cache.size.load(Ordering::Relaxed), 0);
    }

    #[rstest]
    #[test_log::test(tokio::test)]
    async fn block_get_invalid(cache: CacheFixture) {
        // we only allow keys > 0 len
        let requests: &[Arc<[u8]>] = &[b""[..].into()];

        cache.block_get(requests).locations.iter().for_each(|result| {
            assert_matches!(result.as_ref().unwrap_err(), Error::KeyEmpty);
        });
    }

    #[rstest]
    #[test_log::test(tokio::test)]
    async fn block_get(cache: CacheFixture) {
        // put data
        let key1: Arc<[u8]> = Arc::from(&b"potat0e"[..]);
        let data1 = b"hello, world";
        let location1 = cache
            .put1(
                &PutRequest::new(&key1, data1.len() as u64, 0, None),
                0,
                &cache.client_id,
            )
            .unwrap();
        assert_eq!(location1.offset, 0);
        assert_eq!(location1.size, data1.len() as u64);

        // write data into file
        let path = cache.file_pool.base_path().join(&location1.path);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .read(false)
            .truncate(false)
            .open(path)
            .unwrap();
        let result = file.write_at(location1.offset, data1).unwrap();
        assert_eq!(result, data1.len());

        // TODO: get should fail with key open for put
        let requests = vec![key1.clone()];
        let result = cache.block_get(&requests);
        assert_eq!(result.locations.len(), 1);
        assert_matches!(result.locations[0].as_ref().unwrap_err(), Error::KeyOpenForPut(_));

        // commit put
        assert_eq!(cache.size.load(Ordering::Relaxed), 0);
        cache.commit1_deprecated(&key1, 0).unwrap();
        assert_eq!(cache.size.load(Ordering::Relaxed), data1.len() as u64);

        // should be able to get key now
        let result = cache.block_get(&requests);
        assert_eq!(result.locations.len(), 1);
        assert!(result.locations[0].is_ok());
        let location1 = result.locations[0].as_ref().unwrap().as_ref().unwrap();
        assert_eq!(location1.id, 0);
        assert_eq!(location1.offset, 0);
        assert_eq!(location1.size, data1.len() as u64);
        assert_eq!(location1.path, FilePool::file_path(location1.id, &None));

        let path = result.base.join(&location1.path);
        let metadata = fs::metadata(&path).unwrap();
        assert!(metadata.len() >= location1.size);

        // read data from get
        let file = fs::OpenOptions::new()
            .write(false)
            .read(true)
            .truncate(false)
            .open(path)
            .unwrap();
        let mut buf = vec![0u8; data1.len()];
        let file_result = file.read_at(location1.offset, &mut buf).unwrap();
        assert_eq!(file_result, data1.len());
        assert_eq!(buf, data1);

        // close files
        let locations: Vec<_> = result.locations.into_iter().filter_map(Result::ok).flatten().collect();
        assert_eq!(locations.len(), requests.len());
        let result = cache.block_close_deprecated(std::slice::from_ref(&key1));
        let result: Result<()> = result.into_iter().collect();
        assert!(result.is_ok());

        // check key not in use
        assert!(!cache.open_records.contains_key(&key1));
    }

    #[rstest]
    #[test_log::test(tokio::test)]
    async fn max_size(cache: CacheFixture) {
        // valid blocks sizes
        let requests = vec![
            PutRequest::new("blk1", cache.max_size.get() - 1, 0, None),
            PutRequest::new("blk2", cache.max_size.get(), 0, None),
        ];

        let result = cache.block_put(&requests, &cache.client_id).unwrap();
        let locations: Vec<_> = result.locations.into_iter().filter_map(Result::ok).collect();
        assert_eq!(locations.len(), requests.len());

        let result = cache
            .block_abort_deprecated(&[b"blk1"[..].into(), b"blk2"[..].into()])
            .unwrap();
        let result: Result<()> = result.into_iter().collect();
        assert!(result.is_ok());

        // block / files sizes too large
        let requests = vec![
            PutRequest::new("blk3", cache.max_size.get() + 1, 0, None),
            PutRequest::new("blk4", cache.max_size.get() + 2, 0, None),
        ];

        cache
            .block_put(&requests, &cache.client_id)
            .unwrap()
            .locations
            .iter()
            .for_each(|result| {
                assert_matches!(result.as_ref().unwrap_err(), Error::KeyTooLarge(_, _, _));
            });

        // max size
        let request = PutRequest::new("file1", cache.max_size.get(), 0, Some(Arc::from("yaml")));
        let result = cache.file_put(&request, &cache.client_id);
        assert!(result.is_ok());

        let result = cache.file_remove_deprecated(&b"file1"[..].into());
        assert!(result.is_ok());

        // max size + 1
        let request = PutRequest::new("file1", cache.max_size.get() + 1, 0, Some(Arc::from("mdl")));
        let result = cache.file_put(&request, &cache.client_id);
        assert_matches!(result.unwrap_err(), Error::KeyTooLarge(_, _, _));

        // at this point we should have no records in index, no open records
        assert_eq!(cache.index.len(), 0);
        assert_eq!(cache.open_records.len(), 0);
    }

    #[rstest]
    #[test_log::test(tokio::test)]
    async fn file_put(_cache: CacheFixture) {
        // TODO
    }

    #[rstest]
    #[test_log::test(tokio::test)]
    async fn file_get(_cache: CacheFixture) {
        // TODO
    }

    #[rstest]
    #[test_log::test(tokio::test)]
    async fn clear(cache: CacheFixture) {
        let result = cache.clear().await.unwrap();
        assert_eq!(result, 0);

        let data: Vec<Arc<str>> = vec![
            Arc::from("foo"),
            Arc::from("hello, world"),
            Arc::from("0123456789-abc123!@#$%^"),
        ];

        let requests = vec![
            PutRequest::new("k1", data[0].len() as u64, 0, None),
            PutRequest::new("foo-bar", data[1].len() as u64, 0, None),
            PutRequest::new("key", data[2].len() as u64, 0, None),
        ];

        // attempt to put 3 items
        let result = cache.block_put(&requests, &cache.client_id).unwrap();
        let locations: Vec<_> = result.locations.into_iter().filter_map(Result::ok).collect();
        assert_eq!(locations.len(), requests.len());
        let result = cache
            .block_commit_deprecated(&[b"k1"[..].into(), b"foo-bar"[..].into(), b"key"[..].into()])
            .unwrap();
        let commit_results: Vec<_> = result.into_iter().filter_map(Result::ok).collect();
        assert_eq!(commit_results.len(), 3);

        let result = cache.clear().await.unwrap();
        assert_eq!(result, requests.len() as u64);

        // cache should be empty at this point
        assert!(cache.index.is_empty());
        assert_eq!(cache.size.load(Ordering::Relaxed), 0);

        let data: Vec<&[u8]> = vec![b"foo", b"hello, world", b"0123456789-abc123!@#$%^"];

        let requests = vec![
            PutRequest::new("k123456", data[0].len() as u64, 0, None),
            PutRequest::new("foo-bar", data[1].len() as u64, 0, None),
            PutRequest::new("keykey-key key", data[2].len() as u64, 0, None),
        ];

        // attempt to put 3 items
        let result = cache.block_put(&requests, &cache.client_id).unwrap();
        let locations: Vec<_> = result.locations.into_iter().filter_map(Result::ok).collect();
        assert_eq!(locations.len(), requests.len());

        // leave one item
        let result = cache
            .block_commit_deprecated(&[b"k123456"[..].into(), b"foo-bar"[..].into()])
            .unwrap();
        assert_eq!(result.len(), requests.len() - 1);

        let result = cache.clear().await.unwrap();
        assert_eq!(result, requests.len() as u64 - 1);

        // cache should single key left, size is still 0 because no commit1() yet
        assert_eq!(cache.index.len(), 1);
        assert_eq!(cache.size.load(Ordering::Relaxed), 0);

        let result = cache.block_commit_deprecated(&[b"keykey-key key"[..].into()]).unwrap();
        assert_eq!(result.len(), 1);

        assert_eq!(cache.index.len(), 1);
        assert_eq!(cache.size.load(Ordering::Relaxed), data[2].len() as u64);

        let requests = vec![b"keykey-key key"[..].into()];
        let result = cache.block_get(&requests);
        let locations: Vec<_> = result
            .locations
            .into_iter()
            .filter_map(|x| x.ok().unwrap_or(None))
            .collect();
        assert_eq!(locations.len(), 1);

        // attempt clear, but only entry in-use
        let result = cache.clear().await.unwrap();
        assert_eq!(result, 0);

        // close it
        let result = cache.block_close_deprecated(&requests);
        assert_eq!(result.len(), 1);

        // attempt clear, key no longer in use, should clear/remove it
        let result = cache.clear().await.unwrap();
        assert_eq!(result, 1);

        // now things should be empty
        assert_eq!(cache.index.len(), 0);
        assert_eq!(cache.size.load(Ordering::Relaxed), 0);
    }

    #[rstest]
    #[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
    async fn clock_update(cache: CacheFixture) {
        // if clock drifts during test, this would fail...
        let now = cache.now.get();
        // wait long enough time  so that the clock update runs (even under weird VM CI/CD scenarios)
        sleep(Duration::from_millis(2500)).await;
        // drive the clock update once
        cache.clock_update().unwrap();
        trace!(new = cache.now.get(), old = now);
        assert!(cache.now.get() > now);
    }

    #[rstest]
    #[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
    async fn gc_empty(cache: CacheFixture) {
        // gc with empty cache
        let collected = cache.gc().await.unwrap();
        assert_eq!(collected, 0);

        // grab the list of files, there should at exactly one data file, the current file
        let (data_files, unknown_files) = cache.file_pool.files().unwrap();
        assert_eq!(data_files.len(), 1);
        assert_eq!(unknown_files.len(), 0);
    }

    #[rstest]
    #[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
    async fn gc_small(cache: CacheFixture) {
        // fill up one file
        let request = PutRequest::new("k1", cache.data_file_size.as_u64(), 0, None);
        let result = cache.put1(&request, 0, &cache.client_id);
        assert!(result.is_ok());
        let result = cache.commit1_deprecated(&request.key, 1);
        assert!(result.is_ok());

        // partial second
        let request = PutRequest::new("k2", cache.data_file_size.as_u64() / 2, 2, None);
        let result = cache.put1(&request, 0, &cache.client_id);
        assert!(result.is_ok());
        let result = cache.commit1_deprecated(&request.key, 3);
        assert!(result.is_ok());

        // gc with empty cache
        let collected = cache.gc().await.unwrap();
        assert_eq!(collected, 0);

        // grab the list of files, there should at exactly two data files, k1 and the current file
        let (data_files, unknown_files) = cache.file_pool.files().unwrap();
        assert_eq!(data_files.len(), 2);
        assert_eq!(unknown_files.len(), 0);
    }

    #[rstest]
    #[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
    async fn gc_unique_files(cache: CacheFixture) {
        // add file
        let request = PutRequest::new("k1", 10, 0, Some(Arc::from("bat")));
        let result = cache.put1(&request, 0, &cache.client_id);
        assert!(result.is_ok());
        let result = cache.commit1_deprecated(&request.key, 1);
        assert!(result.is_ok());

        // grab the list of files, there should at exactly two data files, k1.bat and the current file
        let (data_files, unknown_files) = cache.file_pool.files().unwrap();
        assert_eq!(data_files.len(), 2);
        assert_eq!(unknown_files.len(), 0);

        // gc with empty cache
        let collected = cache.gc().await.unwrap();
        assert_eq!(collected, 0);

        // no files should be collected at this point still
        let (data_files, unknown_files) = cache.file_pool.files().unwrap();
        assert_eq!(data_files.len(), 2);
        assert_eq!(unknown_files.len(), 0);
    }

    #[rstest]
    #[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
    async fn gc_large(cache: CacheFixture) {
        let mut total_block_size = 0;
        let requests = vec![
            (PutRequest::new("k1", 1024 * 4, 0, None), 0),
            (PutRequest::new("foo-bar", 256 * 1024, 0, None), 1),
            (PutRequest::new("key1", 2 * 1024 * 1024, 0, None), 1),
            (PutRequest::new("key2", 2 * 1024 * 1024, 0, None), 1),
            (PutRequest::new("key3", 2 * 1024 * 1024, 0, None), 1),
            (PutRequest::new("0123451v1", 10 * 1024, 0, None), 1),
            (PutRequest::new("0123452v1", 20 * 1024, 0, None), 1),
            (PutRequest::new("0123453v1", 30 * 1024, 0, None), 1),
            (PutRequest::new("0123454v1", 40 * 1024, 0, None), 1),
            (PutRequest::new("0123455v1", 50 * 1024, 0, None), 1),
            (PutRequest::new("0123456v1", 60 * 1024, 0, None), 1),
            (PutRequest::new("0123457v1", 70 * 1024, 0, None), 1),
            (PutRequest::new("0123458v1", 80 * 1024, 0, None), 1),
            (PutRequest::new("0123459v1", 90 * 1024, 0, None), 1),
            (PutRequest::new("0123451v2", 10 * 1024, 0, None), 2),
            (PutRequest::new("0123452v2", 20 * 1024, 0, None), 2),
            (PutRequest::new("0123453v2", 30 * 1024, 0, None), 2),
            (PutRequest::new("0123454v2", 40 * 1024, 0, None), 2),
            (PutRequest::new("0123455v2", 50 * 1024, 0, None), 2),
            (PutRequest::new("0123456v2", 60 * 1024, 0, None), 2),
            (PutRequest::new("0123457v2", 70 * 1024, 0, None), 2),
            (PutRequest::new("0123458v2", 80 * 1024, 0, None), 2),
            (PutRequest::new("0123459v2", 90 * 1024, 0, None), 2),
            (PutRequest::new("key00", 256 * 1024, 0, None), 5),
            (PutRequest::new("key01", 256 * 1024, 0, None), 5),
            (PutRequest::new("key02", 256 * 1024, 0, None), 5),
            (PutRequest::new("key03", 256 * 1024, 0, None), 5),
            (PutRequest::new("key04", 256 * 1024, 0, None), 5),
            (PutRequest::new("key05", 256 * 1024, 0, None), 5),
            (PutRequest::new("key06", 256 * 1024, 0, None), 5),
            (PutRequest::new("key07", 256 * 1024, 0, None), 5),
            (PutRequest::new("key08", 256 * 1024, 0, None), 5),
            (PutRequest::new("key09", 256 * 1024, 0, None), 6),
        ];

        for request in requests.iter() {
            let result = cache.put1(&request.0, request.1, &cache.client_id);
            assert!(result.is_ok());
            let result = cache.commit1_deprecated(&request.0.key, request.1);
            assert!(result.is_ok());

            total_block_size += request.0.size;
        }

        let mut total_file_size = 0;
        let file_requests = [
            PutRequest::new("file_000", 1, 0, Some(Arc::from("txt"))),
            PutRequest::new("file_001", 1, 0, Some(Arc::from("txt"))),
            PutRequest::new("file_002", 1, 0, Some(Arc::from("txt"))),
            PutRequest::new("file_003", 1, 0, Some(Arc::from("txt"))),
        ];

        for file_request in file_requests.iter() {
            // update the clock
            sleep(Duration::from_millis(25)).await;
            cache.clock_update().unwrap();

            let result = cache.file_put(file_request, &cache.client_id).unwrap();
            let mut file = fs::OpenOptions::new().create(false).write(true).open(&result).unwrap();
            file.write_all(b"Hello, world!").unwrap();
            cache.file_close_deprecated(&file_request.key).unwrap();

            total_file_size += file.metadata().unwrap().len();
        }

        assert!(!cache.index.is_empty());
        assert_eq!(
            cache.entries.load(Ordering::Relaxed),
            requests.len() as u64 + file_requests.len() as u64
        );
        assert_eq!(cache.size.load(Ordering::Relaxed), total_block_size + total_file_size);

        // set max size > current size and verify nothing was collected
        cache.max_size.set(total_block_size + total_file_size + 1);
        let collected = cache.gc().await.unwrap();
        assert_eq!(collected, 0);
        assert_eq!(
            cache.entries.load(Ordering::Relaxed),
            requests.len() as u64 + file_requests.len() as u64
        );
        assert_eq!(cache.size.load(Ordering::Relaxed), total_block_size + total_file_size);

        // set max size == current size and verify nothing was collected
        cache.max_size.set(total_block_size + total_file_size);
        let collected = cache.gc().await.unwrap();
        assert_eq!(collected, 0);
        assert_eq!(
            cache.entries.load(Ordering::Relaxed),
            requests.len() as u64 + file_requests.len() as u64
        );
        assert_eq!(cache.size.load(Ordering::Relaxed), total_block_size + total_file_size);

        // attempt to gc just the first key, threshold == 0, size - 1
        cache.garbage_collection_threshold.set(0);
        cache.max_size.set(total_block_size + total_file_size - 1);
        let collected = cache.gc().await.unwrap();
        assert_eq!(collected, requests[0].0.size);
        assert_eq!(
            cache.entries.load(Ordering::Relaxed),
            requests.len() as u64 + file_requests.len() as u64 - 1
        );
        assert_eq!(
            cache.size.load(Ordering::Relaxed),
            total_block_size + total_file_size - requests[0].0.size
        );

        // open 1 block (prevent gc from collecting)
        // set max size to 0 and attempt to collect everything
        cache.garbage_collection_threshold.set(0);
        cache.max_size.set(0);

        cache.clock_update().unwrap();
        let result = cache.get1(&requests[1].0.key, None, cache.now.get()).unwrap();
        assert!(result.is_some());
        let result = cache
            .get1(
                &file_requests[1].key,
                file_requests[1].extension.clone(),
                cache.now.get(),
            )
            .unwrap();
        assert!(result.is_some());
        let file_size = result.unwrap().size;

        let total_size = cache.size.load(Ordering::Relaxed);
        let collected = cache.gc().await.unwrap();
        trace!(total_size, collected, block_size = requests[1].0.size, file_size);

        assert_eq!(collected, total_size - requests[1].0.size - file_size);
        assert_eq!(cache.entries.load(Ordering::Relaxed), 2);
        assert_eq!(cache.size.load(Ordering::Relaxed), requests[1].0.size + file_size);

        // should only be 3 files left this point (2 open files, 1 current)
        let (data_files, unknown_files) = cache.file_pool.files().unwrap();
        assert_eq!(data_files.len(), 3);
        assert_eq!(unknown_files.len(), 0);

        // close entries and retry gc
        cache.close1_deprecated(&requests[1].0.key).unwrap();
        cache.close1_deprecated(&file_requests[1].key).unwrap();

        let total_size = cache.size.load(Ordering::Relaxed);
        let collected = cache.gc().await.unwrap();
        assert_eq!(collected, total_size);
        assert_eq!(cache.entries.load(Ordering::Relaxed), 0);
        assert_eq!(cache.size.load(Ordering::Relaxed), 0);

        // should only be the current file left
        let (data_files, unknown_files) = cache.file_pool.files().unwrap();
        assert_eq!(data_files.len(), 1);
        assert_eq!(unknown_files.len(), 0);
    }

    #[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
    async fn gc_abandoned_put() {
        let tmp_dir = tempdir().unwrap();
        let key0: Arc<[u8]> = Arc::from(&b"key0"[..]);

        {
            let cache = CacheFixture::new(tmp_dir.path());

            // Put a file size block, but don't commit
            let result = cache.put1(
                &PutRequest::new(key0.clone(), cache.max_size.get(), 0, None),
                1,
                &cache.client_id,
            );
            assert!(result.is_ok());
            assert!(!cache.index.is_empty());
        }

        {
            let cache = CacheFixture::new(tmp_dir.path());

            match cache.open_records.entry(key0) {
                Entry::Occupied(entry) => match entry.get() {
                    OpenRecord::Get { .. } => panic!("get key"),
                    OpenRecord::Put { .. } => panic!("put key"),
                    OpenRecord::Abandoned { value: _, gc_intervals } => {
                        assert_eq!(gc_intervals, &0);
                    },
                },
                Entry::Vacant(_) => panic!("missing key"),
            }

            // Redo put with a different client
            let result = cache.put1(
                &PutRequest::new("key0", cache.data_file_size.as_u64(), 0, None),
                1,
                &Uuid::new_v4(),
            );
            assert!(result.is_ok());
            assert!(!cache.index.is_empty());
            let result = cache.commit1_deprecated(&b"key0"[..].into(), 1);
            assert!(result.is_ok());

            // collect all written files, there should be two
            let (data_files, _) = cache.file_pool.files().unwrap();
            trace!(?data_files, "pre-gc allocated files");
            assert_eq!(data_files.len(), 3);

            // open 1 file (prevent gc)
            let result = cache.get1(&b"key0"[..].into(), None, 1);
            assert!(result.is_ok());

            // Run GC, which should collect the first file
            let _collected = cache.gc().await.unwrap(); // TODO: assert collected bytes size

            // Check for file being cleaned
            let (data_files, _) = cache.file_pool.files().unwrap();
            trace!(?data_files, "post-gc allocated files");
            assert_eq!(data_files.len(), 2);
        }
    }

    #[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
    async fn reconnect() {
        use tokio::time::{sleep, Duration};
        use tracing::warn;

        let tmp_dir = tempdir().unwrap();

        for iter in 1..=10 {
            warn!(iter, "create cache");
            let mut cache = CacheFixture::new(tmp_dir.path());
            cache.run_tasks().unwrap();
            sleep(Duration::from_millis(1000)).await;

            warn!(iter, "put");
            let result = cache.put1(
                &PutRequest::new("key0", cache.data_file_size.as_u64(), 0, None),
                iter,
                &cache.client_id,
            );
            assert!(result.is_ok());
            assert!(!cache.index.is_empty());

            warn!(iter, "dump");
            cache.dump_database().await.unwrap();

            warn!(iter, "shutdown");
            cache.background_tasks.unwrap().shutdown().await;
        }
    }
}

// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Abstraction that allows a database engine to be adapted to the requirements for this storage
//! application.

use std::{
    cell::Cell,
    collections::HashMap,
    fmt::{Debug, Display},
    marker::PhantomData,
    rc::Rc,
    sync::Arc,
    thread::LocalKey,
};

use ovdc_bytes::keys;

pub(crate) mod multi_part_guard;
pub(crate) use multi_part_guard::GuardMap;
pub use multi_part_guard::MultiPartGuard;

use crate::metadata::Metadata;

/// An error produced during a database operation.
pub enum Error {
    /// The requested key could not be found or the end of the db was reached.
    NotFound,

    /// The engine is asynchronously processing the request.
    Busy,

    /// A multi-part write is in progress.
    WriteInProgress,

    /// Could not put the pair because a value already exists.
    Exists,

    /// An inner database error occurred.
    Engine(Box<dyn Display + Send + Sync + 'static>),
}

impl Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotFound => write!(f, "NotFound: the key was not found in the db"),
            Error::Busy => write!(
                f,
                "Busy: the engine is busy and cannot currently process the request."
            ),
            Error::WriteInProgress => write!(
                f,
                "Write in progress: another task is currently writing to the given key"
            ),
            Error::Exists => write!(f, "Exists: value already exists at this key"),
            Error::Engine(e) => write!(f, "Engine: {}", e),
        }
    }
}

impl Debug for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Error: ")?;
        Display::fmt(self, f)
    }
}

impl std::error::Error for Error {}

/// Instructs the merge operator how to change the Metadata.
#[derive(
    Default,
    Clone,
    Copy,
    serde::Serialize,
    serde::Deserialize,
    Debug,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]

pub enum Change<T> {
    /// Do nothing
    #[default]
    No,
    /// Set the value.
    Set(T),
}

/// The metadata fields of a key that can be modified.
///
/// When a field is non None, the field is updated for the key.
/// Fields set to None are not modified.
#[derive(
    Default,
    Clone,
    Copy,
    serde::Serialize,
    serde::Deserialize,
    Debug,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]

pub struct Update {
    /// Changes the access time for a key-value pair.
    ///
    /// Access time only moves forward. An update carrying a time older than the
    /// stored one is ignored: updates are queued in memory before they are
    /// applied, and independent flushes race, so they do not arrive in time
    /// order.
    pub access_time: Change<u64>,

    /// Changes the time to live to the given value.
    pub ttl_ms: Change<Option<u64>>,
}

/// Models a value returned by the database.
#[derive(Clone, Debug)]
pub enum Value {
    /// The value is a blob.
    Blob {
        /// The entire size of the value.
        size: u64,
        /// The next key to request.
        next: keys::Compound,
        /// An additional tag that identifies a specific version of a value.
        tag: u128,
        /// The value, if any.
        value: Option<Arc<Vec<u8>>>,
    },
    /// The key-value is inline.
    Vec(Arc<Vec<u8>>),
}

impl Value {
    /// Returns true if the value kind is a blob
    pub fn is_blob(&self) -> bool {
        matches!(self, Self::Blob { .. })
    }

    /// Returns true if there is no value.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Return the size of this value.
    pub fn len(&self) -> u64 {
        match self {
            Self::Blob { value, .. } => value.as_ref().map(|y| y.len()).unwrap_or_default() as u64,
            Self::Vec(value) => value.len() as u64,
        }
    }

    /// Returns the binary value if any.
    pub fn slice(&self) -> Option<&[u8]> {
        match self {
            Self::Blob { value, .. } => value.as_ref().map(|y| &***y),
            Self::Vec(value) => Some(&***value),
        }
    }

    /// Returns the next key to use when looking for another blob.
    pub fn next_blob_key(&self) -> Option<keys::Compound> {
        match self {
            Self::Blob { next, .. } => Some(*next),
            _ => None,
        }
    }
}

#[allow(missing_docs)]
#[derive(Clone, Debug)]
pub struct AnyEntry {
    pub(crate) key: keys::Compound,
    pub(crate) value: Value,
    pub(crate) metadata: Metadata,
}

impl Entry for AnyEntry {
    fn key(&self) -> keys::Compound {
        self.key
    }

    fn value(&self) -> &Value {
        &self.value
    }

    fn metadata(&self) -> &Metadata {
        &self.metadata
    }
}

/// A key-value pair.
pub trait Entry: Send + Sync {
    /// The key of the key-value pair.
    fn key(&self) -> keys::Compound;

    /// The value bytes of the pair.
    fn value(&self) -> &Value;

    /// The metadata for the pair
    fn metadata(&self) -> &Metadata;
}

/// A view into the database.
///
/// A cursor does not represent a consistent view. The database is allowed to change while a cursor
/// is held.
pub trait Cursor: MutCursor + Send + Sync {
    /// Get a single entry by key.
    ///
    /// If the database is being iterated this function is allowed to move the iterator to the
    /// given key.
    fn get(&self, key: keys::Compound) -> Result<AnyEntry, Error>;

    /// Get a single entry by key, without performing disk IO.
    ///
    /// Returns the entry IFF it can be served without IO or decompression, and
    /// an error otherwise. An error does not mean the key is absent -- only
    /// that serving it would have required IO.
    fn get_non_blocking(&self, key: keys::Compound) -> Result<AnyEntry, Error>;

    /// Returns OK IFF the function found the given key. Otherwise an error is returned.
    fn exists(&self, key: keys::Compound) -> Result<(), Error>;

    /// Returns OK IFF the function found the given key in-memory. Otherwise an error is returned.
    ///
    /// This function will not perform any disk-io. The key may still exist.
    fn exists_non_blocking(&self, key: keys::Compound) -> Result<(), Error>;
}

thread_local! {
    static FLAGS: Cell<u8> = const {Cell::new(0)};
}

/// When dropped, automatically resets the thread local flags.
pub struct ThreadLocalGuard(&'static LocalKey<Cell<u8>>, PhantomData<Rc<()>>);

impl Drop for ThreadLocalGuard {
    fn drop(&mut self) {
        self.0.set(0);
    }
}

/// Can be used to decrease the speed of writes but increase the durability, or the guarantee that
/// the written data will be retained beyond an application crash/restart or an OS crash/restart.
pub const FLAGS_HIGHER_DURABILITY: u8 = 1;

/// Sets thread local flags for the cursor operation.
pub fn thread_local_flags(flags: u8) -> ThreadLocalGuard {
    FLAGS.set(flags);
    ThreadLocalGuard(&FLAGS, Default::default())
}

/// Gets the local thread flags.
pub(crate) fn get_thread_local_flags() -> u8 {
    FLAGS.get()
}

type BoxFn = Box<dyn Fn(&dyn Entry) -> bool + Send + Sync + 'static>;

/// A mutable cursor for the database engine that allows it to be modified.
pub trait MutCursor {
    /// Deletes the given key-pair.
    fn delete(&self, key: keys::Compound) -> Result<(), Error>;

    /// Updates the metadata for the given key-value pair.
    ///
    /// This does not check that the key exists, and returns `Ok` either way --
    /// it cannot be used as an existence check. Updating a key that does not
    /// exist leaves an unreadable row behind until the next [`MutCursor::retain`]
    /// pass collects it.
    fn update(&self, key: keys::Compound, mut_meta: Update) -> Result<(), Error>;

    /// Updates multiple keys at once.
    ///
    /// This may be more efficient than calling update if many keys will be updated.
    fn update_many(&self, updates: Vec<(keys::Compound, Update)>) -> Result<(), Error> {
        for (key, mut_meta) in updates {
            self.update(key, mut_meta)?;
        }
        Ok(())
    }

    /// Insert a new key-pair into the database.
    fn put(&self, key: keys::Compound, metadata: Metadata, value: &[u8]) -> Result<(), Error>;

    /// Given the filter `with`, the database will be removed of all values for which the function
    /// returns false.
    ///
    /// The database will retain only items for which the function returns true.
    fn retain(&self, with: BoxFn) -> Result<(), Error>;
}

/// On-disk footprint of the database engine, split by what the bytes represent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stat {
    /// Bytes held by SST + blob files — the stored key/value content. This is the
    /// component that grows when new data is flushed to disk.
    pub content: u64,
    /// Bytes held by everything else in the database directory (WAL, MANIFEST,
    /// CURRENT, OPTIONS, ...). This term is not monotonic: a flush can recycle
    /// WAL files, shrinking it even as `content` grows.
    pub metadata: u64,
    /// Total on-disk footprint (`content + metadata`).
    pub total: u64,
}

/// When set, the service will store blobs in a new table.
pub const FLAG_DB_BLOB_ENABLE: u8 = 1;

/// When set, the service stores values as an append-only log per key, collapsed
/// by a merge operator, instead of overwriting a single stored value.
///
/// This selects a different on-disk value encoding and merge operator, so it
/// cannot be changed for an environment that already holds data.
pub const FLAG_DB_LOG_ENABLE: u8 = 1 << 1;

/// Describes the basic functions of a database.
pub trait Database: BlobProvider + Clone + Send + Sync + 'static {
    /// Opens the database environment at the given path.
    ///
    /// The database capacity/free percentage will be calculated using the given size. For some
    /// database engines, the database itself may not be restricted to the size. Meaning that the
    /// database can fill the device that the environment lives on.
    fn new(path: &str, size: usize, flags: u8, config: &ovdc_settings::Cfg) -> Result<Self, Error>;

    /// Opens a cursor in to the database engine.
    fn open(&self) -> &dyn Cursor;

    /// Allows collection of DB specific properties.
    fn properties(&self) -> HashMap<PropertyId, PropertyValue>;

    /// Reports the on-disk footprint of the database engine, split into
    /// `content` (SST + blob files), `metadata` (WAL and other bookkeeping
    /// files), and their `total`.
    fn stat(&self) -> Result<Stat, Error>;

    /// Gracefully closes the database engine.
    fn close(&self) -> Result<(), Error>;

    /// Some storage engines may prevent writes for some period of time. This
    /// function returns false when that is the case, and true when writes are
    /// expected to proceed.
    ///
    /// This is a hint, not a guarantee: a write may still stall after this
    /// returns true. It allows higher layers to optimize around write stalls.
    fn can_write_hint(&self) -> bool;
}

/// Used to obtain a Binary Large Object (BLOB) reader and writer.
pub trait BlobProvider: Send + Sync {
    /// Read and write BLOBs.
    fn blobs(&self) -> &dyn Blob;
}

/// A shared type for both blob contexts.
pub type BlobContext = Box<dyn std::any::Any + Send + Sync + 'static>;

#[allow(missing_docs)]
pub trait Blob: Read + Write {
    fn begin_read(&self, key: keys::Compound, tag: u128) -> BlobContext;

    fn begin_write(
        &self,
        key: keys::Compound,
        metadata: Metadata,
        size: u64,
    ) -> Option<(BlobContext, u128)>;

    /// The largest number of parts a blob may be split into.
    ///
    /// Scheduling beyond this fails rather than wrapping onto an earlier part.
    /// Callers that know the chunk size should check a value against this before
    /// starting a write, so an oversized value is refused up front instead of
    /// part way through.
    fn max_parts(&self) -> u32;

    /// Removes the parts of the blob identified by `key` and `tag`.
    ///
    /// Returns [`Error::Busy`] when the engine is applying backpressure: the
    /// parts are still on disk and the caller should retry. Removing a blob
    /// that is already absent is not an error.
    fn remove(&self, key: keys::Compound, tag: u128) -> Result<(), Error>;
}

#[allow(missing_docs)]
pub trait Read: Send + Sync {
    fn poll_read(&self, cx: &mut BlobContext) -> Result<(), std::io::Error>;
    fn next<'a>(&self, cx: &'a mut BlobContext) -> Option<&'a [u8]>;
}

#[allow(missing_docs)]
pub trait Write: Send + Sync {
    fn schedule(&self, cx: &mut BlobContext, from: &[u8]) -> bool;

    /// Finalises the blob, making the parts written so far readable.
    ///
    /// Returns [`Error::Busy`] when the engine is applying back-pressure; the
    /// blob is unchanged and the call can be retried with the same context.
    ///
    /// Until this succeeds the parts are on disk but nothing refers to them, so
    /// a caller that gives up should drop the context: that releases the write
    /// guard without marking it complete, and the parts are reclaimed.
    fn done(&self, cx: &mut BlobContext) -> Result<(), Error>;
}

#[allow(missing_docs)]
#[derive(Debug, Clone)]
pub enum PropertyValue {
    Bool(bool),
    String(String),
    Number(u64),
    Float(f64),
}

impl PropertyValue {
    /// Value is returned only if it matches the requested type.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Number(x) => Some(*x),
            _ => None,
        }
    }
    /// Value is returned only if it matches the requested type.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(x) => Some(*x),
            _ => None,
        }
    }
    /// Value is returned only if it matches the requested type.
    pub fn as_float(&self) -> Option<f64> {
        match self {
            Self::Float(x) => Some(*x),
            _ => None,
        }
    }
}

impl From<bool> for PropertyValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<String> for PropertyValue {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<u64> for PropertyValue {
    fn from(value: u64) -> Self {
        Self::Number(value)
    }
}

/// Column family holding keys, metadata and inline values.
pub const DEFAULT_CF: &str = "default";

/// Column family holding blob parts, present only under [`FLAG_DB_BLOB_ENABLE`].
pub const BLOBS_CF: &str = "blobs";

/// What part of the engine a property value describes.
///
/// Some engine properties are divided by column family and some are not, and
/// the difference is not visible from the property name. Reading a per-column-
/// family property once and exporting it unqualified attributes the whole
/// database's behaviour to one family; exporting a database-wide property once
/// per family invents detail that does not exist.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PropertyScope {
    /// The value covers the whole database and is not divided by column family.
    Database,

    /// The value describes the named column family alone.
    ColumnFamily(&'static str),
}

impl PropertyScope {
    /// The value used for the exported `cf` label.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Database => "-",
            Self::ColumnFamily(name) => name,
        }
    }
}

/// Identifies one exported property series.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct PropertyId {
    /// The property name.
    pub name: &'static str,

    /// What the value describes.
    pub scope: PropertyScope,
}

impl PropertyId {
    /// A property covering the whole database.
    pub fn database(name: &'static str) -> Self {
        Self {
            name,
            scope: PropertyScope::Database,
        }
    }

    /// A property describing one column family.
    pub fn column_family(name: &'static str, cf: &'static str) -> Self {
        Self {
            name,
            scope: PropertyScope::ColumnFamily(cf),
        }
    }
}

pub(crate) trait Sealed {}

#[allow(missing_docs, private_bounds)]
pub trait PropertyKind: Sealed {
    type Kind;

    fn name(&self) -> &'static str;

    /// Looks up this property's headline value.
    ///
    /// For a database-wide property that is the only value. For one divided by
    /// column family it is the `default` family's, which is what this returned
    /// before per-family values were exported; use [`PropertyKind::extract_cf`]
    /// to reach another family.
    #[inline(always)]
    fn extract<'a>(
        &self,
        map: &'a HashMap<PropertyId, PropertyValue>,
    ) -> Option<&'a PropertyValue> {
        map.get(&PropertyId::database(self.name()))
            .or_else(|| map.get(&PropertyId::column_family(self.name(), DEFAULT_CF)))
    }

    /// Looks up this property's value for one column family.
    #[inline(always)]
    fn extract_cf<'a>(
        &self,
        map: &'a HashMap<PropertyId, PropertyValue>,
        cf: &'static str,
    ) -> Option<&'a PropertyValue> {
        map.get(&PropertyId::column_family(self.name(), cf))
    }
}

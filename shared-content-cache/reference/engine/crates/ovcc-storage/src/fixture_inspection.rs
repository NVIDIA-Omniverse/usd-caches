// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Read-only evidence for the standalone upgrade fixture; excluded from release builds.

use super::*;

const PREFIX: &str = "|az.mock.local|GET|/optional-key-upgrade/";

/// Actual persisted records and physical mappings, with streamed file fingerprints.
#[derive(Debug, Serialize)]
pub struct FixtureSnapshot {
    pub entries: Vec<FixtureEntry>,
    pub mappings: Vec<FixtureMapping>,
}

/// Schema is read from the stored tag before decoding.
#[derive(Debug, Serialize)]
pub struct FixtureEntry {
    pub schema: u16,
    pub record: MetadataRecord,
    pub files: Vec<FixtureFile>,
    /// Payload fingerprint for byte-preservation checks across upgrades.
    pub payload_sha256: Option<String>,
}

/// Durable mapping read from the physical-key index, not inferred from metadata.
#[derive(Debug, Serialize, Deserialize)]
pub struct FixtureMapping {
    pub physical_key: String,
    pub owner_key: String,
    pub revision: u64,
}

/// Byte identity and filesystem identity of one contiguous body or segment file.
#[derive(Debug, Serialize)]
pub struct FixtureFile {
    pub path: String,
    pub inode: u64,
    pub length: u64,
    pub sha256: String,
}

fn files(root: &Path, path: &Path, output: &mut Vec<FixtureFile>) -> Result<()> {
    let (meta, identity) = match ovcc_platform::stat_nofollow(path) {
        Ok(stat) => (stat.metadata, stat.identity),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if meta.is_dir() {
        for entry in fs::read_dir(path)? {
            files(root, &entry?.path(), output)?;
        }
    } else if meta.is_file() {
        let mut source = File::open(path)?;
        output.push(FixtureFile {
            path: path
                .strip_prefix(root)
                .map_err(|error| Error::storage(error.to_string()))?
                .to_string_lossy()
                .into(),
            // Linux inodes and NTFS file ids fit in 64 bits; only ReFS ids
            // would truncate, which merely weakens this evidence field.
            inode: identity.file as u64,
            length: meta.len(),
            sha256: payload_hash(&mut source)?,
        });
    } else {
        return Err(Error::storage(
            "fixture body is not a regular file/directory",
        ));
    }
    output.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(())
}

fn payload_hash(source: &mut File) -> Result<String> {
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn snapshot(root: &Path, txn: &redb::ReadTransaction) -> Result<FixtureSnapshot> {
    let mut entries = Vec::new();
    let table = match txn.open_table(METADATA) {
        Ok(table) => Some(table),
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(error) => return Err(map_redb(error)),
    };
    if let Some(table) = table {
        for item in table.iter().map_err(map_redb)? {
            let (key, value) = item.map_err(map_redb)?;
            if !key.value().contains(PREFIX) {
                continue;
            }
            let raw = value.value();
            let tag: [u8; 2] = raw
                .get(..2)
                .ok_or_else(|| Error::storage("missing schema tag"))?
                .try_into()
                .map_err(|_| Error::storage("invalid schema tag"))?;
            let record = decode_record(raw)?
                .ok_or_else(|| Error::storage("unsupported fixture metadata schema"))?;
            let hex = sha256_hex(&record.key);
            let (shard, name) = hex.split_at(2);
            let name = match record.layout {
                ObjectLayout::Contiguous => return Err(Error::storage("retired object layout")),
                ObjectLayout::Segmented => format!("{name}.segments"),
                ObjectLayout::Raw => format!("{name}.raw"),
            };
            let mut body_files = Vec::new();
            let body_path = root.join("objects").join(shard).join(name);
            files(root, &body_path, &mut body_files)?;
            let payload_sha256 =
                if record.layout != ObjectLayout::Segmented && !body_files.is_empty() {
                    let mut source = File::open(&body_path)?;
                    Some(payload_hash(&mut source)?)
                } else {
                    None
                };
            entries.push(FixtureEntry {
                schema: u16::from_le_bytes(tag),
                record,
                files: body_files,
                payload_sha256,
            });
        }
    }
    let mut mappings = Vec::new();
    let table: TableDefinition<&str, &[u8]> = TableDefinition::new("physical_keys_v1");
    match txn.open_table(table) {
        Ok(table) => {
            #[derive(Deserialize)]
            struct Owner {
                key: String,
                revision: u64,
            }
            for item in table.iter().map_err(map_redb)? {
                let (key, value) = item.map_err(map_redb)?;
                if !key.value().contains(PREFIX) {
                    continue;
                }
                let owner: Owner = postcard::from_bytes(value.value())
                    .map_err(|error| Error::storage(error.to_string()))?;
                mappings.push(FixtureMapping {
                    physical_key: key.value().into(),
                    owner_key: owner.key,
                    revision: owner.revision,
                });
            }
        }
        Err(redb::TableError::TableDoesNotExist(_)) => {}
        Err(error) => return Err(map_redb(error)),
    }
    Ok(FixtureSnapshot { entries, mappings })
}

impl DiskStorage {
    /// Observes through the owning database handle without reopening a live store.
    pub fn inspect_optional_fixture(&self) -> Result<FixtureSnapshot> {
        let _operations = self.inner.write_operations();
        snapshot(
            &self.inner.root,
            &self.inner.db.begin_read().map_err(map_redb)?,
        )
    }
}

/// Reads an offline copy without migrating records or recovering objects.
/// Opens only the supported database format; body reads use the original
/// read-only volume. Historical format inspectors live in the pinned source.
pub fn inspect_offline_optional_fixture(
    root: &Path,
    metadata_copy: &Path,
) -> Result<FixtureSnapshot> {
    let db = Database::open(metadata_copy).map_err(map_redb)?;
    snapshot(root, &db.begin_read().map_err(map_redb)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn general_inspect_payload_fingerprint_survives_restart() -> Result<()> {
        let root = tempfile::tempdir()?;
        let storage = DiskStorage::open(root.path())?;
        let key = "0|az.mock.local|GET|/optional-key-upgrade/raw||etag|";
        let mut writer = storage.begin_fill(key, StoredMeta::default())?;
        writer.write_chunk(b"ab")?;
        drop(writer.commit()?);
        let before = storage.inspect_optional_fixture()?;
        drop(storage);
        let storage = DiskStorage::open(root.path())?;
        let after = storage.inspect_optional_fixture()?;
        assert_eq!(before.entries[0].payload_sha256, Some(sha256_hex("ab")));
        assert_eq!(
            after.entries[0].payload_sha256,
            before.entries[0].payload_sha256
        );
        assert_eq!(
            after.entries[0].files[0].sha256,
            before.entries[0].files[0].sha256
        );
        assert_eq!(after.entries[0].record, before.entries[0].record);
        Ok(())
    }

    #[test]
    fn general_inspect_database_preserves_source_and_record_schema() -> Result<()> {
        // Arrange.
        let root = tempfile::tempdir()?;
        let path = root.path().join("metadata.redb");
        let mut record = crate::tests::full_record();
        record.key = "0|az.mock.local|GET|/optional-key-upgrade/legacy||etag|".into();
        record.state = CacheState::MetadataOnly;
        let encoded = encode_record(&record)?;
        {
            let db = redb::Database::create(&path).map_err(map_redb)?;
            let txn = db.begin_write().map_err(map_redb)?;
            {
                let mut table = txn
                    .open_table(redb::TableDefinition::<&str, &[u8]>::new("metadata"))
                    .map_err(map_redb)?;
                table
                    .insert(record.key.as_str(), encoded.as_slice())
                    .map_err(map_redb)?;
            }
            txn.commit().map_err(map_redb)?;
        }
        let original = fs::read(&path)?;
        let scratch = tempfile::tempdir()?;
        let copy = scratch.path().join("copy.redb");
        fs::write(&copy, &original)?;

        // Act.
        let snapshot = inspect_offline_optional_fixture(root.path(), &copy)?;

        // Assert.
        assert_eq!(snapshot.entries.len(), 1);
        assert_eq!(snapshot.entries[0].schema, METADATA_SCHEMA_VERSION);
        assert_eq!(snapshot.entries[0].record, record);
        assert_eq!(fs::read(&path)?, original);
        drop(Database::open(&path).map_err(map_redb)?);
        Ok(())
    }

    #[test]
    fn general_inspect_empty_baseline_without_metadata_table() -> Result<()> {
        let root = tempfile::tempdir()?;
        let storage = DiskStorage::open(root.path())?;
        assert!(storage.inspect_optional_fixture()?.entries.is_empty());
        drop(storage);
        let scratch = tempfile::tempdir()?;
        let copy = scratch.path().join("copy.redb");
        fs::copy(root.path().join("metadata.redb"), &copy)?;
        let snapshot = inspect_offline_optional_fixture(root.path(), &copy)?;
        assert!(snapshot.entries.is_empty());
        assert!(snapshot.mappings.is_empty());
        Ok(())
    }

    #[test]
    fn general_inspect_offline_copy_preserves_original_schema_and_body() -> Result<()> {
        let root = tempfile::tempdir()?;
        let storage = DiskStorage::open(root.path())?;
        let key = "0|az.mock.local|GET|/optional-key-upgrade/inspect||etag|";
        let mut writer = storage.begin_fill(key, StoredMeta::default())?;
        writer.write_chunk(b"unchanged")?;
        drop(writer.commit()?);
        let online = storage.inspect_optional_fixture()?;
        assert_eq!(online.entries.len(), 1);
        assert_eq!(online.entries[0].schema, METADATA_SCHEMA_VERSION);
        assert_eq!(online.entries[0].files.len(), 1);
        drop(storage);
        let original = fs::read(root.path().join("metadata.redb"))?;
        let scratch = tempfile::tempdir()?;
        let copy = scratch.path().join("copy.redb");
        fs::write(&copy, &original)?;
        let offline = inspect_offline_optional_fixture(root.path(), &copy)?;
        assert_eq!(offline.entries[0].record.key, key);
        assert_eq!(offline.entries[0].schema, online.entries[0].schema);
        assert_eq!(
            offline.entries[0].files[0].inode,
            online.entries[0].files[0].inode
        );
        assert_eq!(
            offline.entries[0].files[0].sha256,
            online.entries[0].files[0].sha256
        );
        assert_eq!(fs::read(root.path().join("metadata.redb"))?, original);
        Ok(())
    }
}

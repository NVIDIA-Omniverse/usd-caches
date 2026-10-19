// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn disk_fill_raw_payload_is_the_entire_file_after_restart() -> Result<()> {
    // Arrange.
    let temp = tempfile::tempdir()?;
    let cases: &[&[u8]] = &[b"", b"x", b"\0\xff\x80binary\r\n", b"OVCCDATA\x01\0payload"];
    let storage = DiskStorage::open(temp.path())?;
    for (index, body) in cases.iter().enumerate() {
        let key = format!("raw-{index}");
        let mut writer = storage.begin_fill(&key, StoredMeta::default())?;

        // Act.
        for chunk in body.chunks(3) {
            writer.write_chunk(chunk)?;
        }
        let lease = writer.commit()?;

        // Assert.
        assert_eq!(fs::read(&lease.path)?, *body);
        assert_eq!(lease.body_offset, 0);
        assert_eq!(lease.file.metadata()?.len(), body.len() as u64);
    }
    drop(storage);

    // Act.
    let storage = DiskStorage::open(temp.path())?;

    // Assert.
    for (index, body) in cases.iter().enumerate() {
        let Lookup::Hit(lease) = storage.lookup(&format!("raw-{index}"))? else {
            return Err(Error::storage("raw payload did not survive restart"));
        };
        assert_eq!(fs::read(&lease.path)?, *body);
        assert_eq!(lease.body_offset, 0);
    }
    Ok(())
}

#[test]
fn disk_recover_raw_fill_rejects_length_changes() -> Result<()> {
    for state in [CacheState::Cached, CacheState::FetchInProgress] {
        for length in [0, 2, 8] {
            // Arrange.
            let temp = tempfile::tempdir()?;
            let storage = DiskStorage::open(temp.path())?;
            let mut writer = storage.begin_fill("raw", StoredMeta::default())?;
            writer.write_chunk(b"bytes")?;
            let lease = writer.commit()?;
            let mut record = storage
                .inner
                .get_record("raw")?
                .ok_or_else(|| Error::storage("missing record"))?;
            record.state = state;
            storage.inner.put_record(&record)?;
            OpenOptions::new()
                .write(true)
                .open(&lease.path)?
                .set_len(length)?;
            drop(lease);
            drop(storage);

            // Act.
            let storage = DiskStorage::open(temp.path())?;

            // Assert.
            assert!(!matches!(storage.lookup("raw")?, Lookup::Hit(_)));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn disk_get_raw_mmap_survives_replacement_and_eviction() -> Result<()> {
    // Arrange.
    let temp = tempfile::tempdir()?;
    let storage = DiskStorage::open(temp.path())?;
    let bytes = b"mapped directly from offset zero";
    let mut writer = storage.begin_fill("mapped", StoredMeta::default())?;
    writer.write_chunk(bytes)?;
    let lease = writer.commit()?;
    let mapping = unsafe { ovcc_platform::ReadOnlyMapping::new(&lease.file, bytes.len())? };

    // Act.
    let mut replacement = storage.begin_fill("mapped", StoredMeta::default())?;
    replacement.write_chunk(b"new")?;
    let replacement = replacement.commit()?;
    assert!(storage.evict_data("mapped", replacement.meta.revision)?);
    drop(lease);

    // Assert.
    assert_eq!(mapping.as_bytes(), bytes);
    Ok(())
}

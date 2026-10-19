// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use async_zip::write::ZipFileWriter;
use async_zip::{Compression, ZipEntryBuilder};
use chrono::prelude::*;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use tokio::fs;
use tracing::{debug, info};
use walkdir::WalkDir;

use crate::Result;

#[derive(Debug)]
pub struct SupportBundleBuilder {
    /// zip file comment
    comment: Option<String>,
    /// list of directories to include in zip archive
    dirs: Vec<(PathBuf, PathBuf)>,
    /// list of files include in zip archive
    files: Vec<(PathBuf, PathBuf)>,
    /// in-memory bytes to include in zip archive
    memory: Vec<(Vec<u8>, PathBuf)>,
}

impl SupportBundleBuilder {
    /// set the zip archive comment (metadata)
    pub fn with_comment(mut self, comment: impl AsRef<str>) -> Self {
        self.comment = Some(comment.as_ref().to_owned());
        self
    }

    /// add a directory to the zip archive
    /// path is path on disk to add
    pub fn add_directory(mut self, input: impl AsRef<Path>, zip: impl AsRef<Path>) -> Self {
        self.dirs.push((input.as_ref().to_owned(), zip.as_ref().to_owned()));
        self
    }

    /// add a file to the zip archive
    /// path is path on disk to add
    /// zip_path is path in the zip archive to add file
    pub fn add_file(mut self, input: impl AsRef<Path>, zip: impl AsRef<Path>) -> Self {
        self.files.push((input.as_ref().to_owned(), zip.as_ref().to_owned()));
        self
    }

    /// add bytes to zip archive
    pub fn add_bytes(mut self, data: Vec<u8>, zip: impl AsRef<Path>) -> Self {
        self.memory.push((data, zip.as_ref().to_owned()));
        self
    }

    async fn add_file_to_zip(
        zip: &mut ZipFileWriter<fs::File>,
        input_path: impl AsRef<Path>,
        file_name: impl AsRef<Path>,
    ) -> Result<()> {
        let mut reader = fs::File::open(input_path).await?;
        let opts = ZipEntryBuilder::new(file_name.as_ref().to_string_lossy().to_string(), Compression::Deflate);
        let mut writer = zip.write_entry_stream(opts).await?;
        tokio::io::copy(&mut reader, &mut writer).await?;
        Ok(writer.close().await?)
    }

    async fn add_bytes_to_zip(
        zip: &mut ZipFileWriter<fs::File>,
        data: &[u8],
        file_name: impl AsRef<Path>,
    ) -> Result<()> {
        let opts = ZipEntryBuilder::new(file_name.as_ref().to_string_lossy().to_string(), Compression::Deflate);
        Ok(zip.write_entry_whole(opts, data).await?)
    }

    pub async fn build(self) -> Result<SupportBundle> {
        let tmp_dir = tempfile::Builder::new().prefix("hub-supportbundle-").tempdir()?;
        let now: DateTime<Utc> = std::time::SystemTime::now().into();
        let file_name = PathBuf::from(format!("hub-supportbundle-{}.zip", now.format("%Y%m%d-%H%M%S-%3f")));
        let path = tmp_dir.path().join(file_name);

        info!(?self, ?path, "building support bundle");

        let mut zip = ZipFileWriter::new(fs::File::create(&path).await?);

        if let Some(comment) = self.comment {
            zip.comment(comment);
        }

        let mut files = Vec::new();

        // copy over files, ignore files that are unreadable
        for (file, zip_path) in self.files {
            if file.is_file() {
                files.push((file, zip_path));
            }
        }

        // iterate directories adding all readable files
        for (dir, zip_path) in self.dirs {
            for entry in WalkDir::new(&dir)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|d| d.path().is_file())
            {
                // strip the dir prefix off and add remaining to zip_path
                let zip_path = zip_path.join(entry.path().strip_prefix(&dir)?);
                files.push((entry.path().to_path_buf(), zip_path));
            }
        }

        // add normalized file paths to zip
        for (file, zip_file) in files {
            debug!(?file, ?zip_file, "adding file/directory to zip");
            Self::add_file_to_zip(&mut zip, file, zip_file).await?;
        }

        // add in-memory data to zip
        for (bytes, zip_file) in self.memory {
            debug!(len = bytes.len(), ?zip_file, "adding in-memory data to zip");
            Self::add_bytes_to_zip(&mut zip, bytes.as_ref(), zip_file).await?;
        }

        // must close zip before its dropped
        zip.close().await?;

        Ok(SupportBundle {
            _tmp_dir: tmp_dir,
            path,
        })
    }
}

#[derive(Debug)]
pub struct SupportBundle {
    /// use a temporary directory so the support bundle is automatically
    /// deleted once it is dropped
    _tmp_dir: TempDir,
    /// path to support bundle
    path: PathBuf,
}

impl SupportBundle {
    pub fn builder() -> SupportBundleBuilder {
        SupportBundleBuilder {
            comment: None,
            dirs: Vec::new(),
            files: Vec::new(),
            memory: Vec::new(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_zip::read::seek::ZipFileReader;
    use rstest::rstest;
    use serde_json::json;
    use tokio::fs;

    #[rstest]
    #[case(0)]
    #[case(1)]
    #[case(2)]
    #[case(10)]
    #[case(100)]
    #[test_log::test(tokio::test)]
    async fn support_bundle(#[case] num_files: usize) {
        // create a temporary directory of files to zip
        let tmp_dir = tempfile::Builder::new().tempdir().unwrap();

        let test_dir = tmp_dir.path().join("test_dir");
        fs::create_dir(&test_dir).await.unwrap();

        for i in 0..num_files {
            let file = test_dir.join(format!("file.{i:03}.txt"));
            fs::write(file, format!("test file data {i} of {num_files}"))
                .await
                .unwrap();
        }

        let test_file = tmp_dir.path().join("test_file.txt");
        fs::write(&test_file, format!("test file data {num_files}"))
            .await
            .unwrap();

        let path = {
            let builder = SupportBundle::builder()
                .with_comment("test bundle")
                .add_directory(test_dir, format!("test_dir_{num_files}"))
                .add_file(test_file, "test_file.txt")
                .add_bytes(json!({ "test": "ok" }).to_string().into_bytes(), "test.json");
            let bundle = builder.build().await.unwrap();

            let path = bundle.path();
            assert!(path.exists());
            assert!(path.is_file());
            assert!(fs::metadata(&path).await.unwrap().len() > 0);

            let archive = fs::File::open(&path).await.unwrap();
            let reader = ZipFileReader::new(archive).await.unwrap();
            assert_eq!(reader.file().entries().len(), num_files + 2);

            path.to_path_buf()
        };

        // after bundle is dropped, file should be deleted automatically
        assert!(!path.exists());
        assert!(!path.parent().unwrap().exists());
    }
}

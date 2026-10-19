// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

mod deathsignal;
use deathsignal::DeathSignal;

use fd_lock::RwLock;
use process_path::get_dylib_path;
use semver::Version;
use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;
use tracing::{debug, info, trace};

use hub_util::settings::Settings;

use crate::{Error, Mode, Result};

#[derive(Debug)]
pub(crate) struct Launcher {
    discovery_lock: RwLock<File>,
    mode: Mode,
    config_file: PathBuf,
}

impl Launcher {
    /// create new instance of launcher for a given mode
    pub(crate) fn new(mode: Mode) -> Result<Self> {
        let discovery_lock_file = env::temp_dir().join(format!("hub-{}.lock", whoami::username()));
        let discovery_lock = RwLock::new(File::create(discovery_lock_file)?);

        let config_file = env::temp_dir().join(format!(
            "hub-{}-{:X}.config.json",
            whoami::username(),
            rand::random::<u32>()
        ));

        Ok(Self {
            discovery_lock,
            mode,
            config_file,
        })
    }

    #[cfg(target_os = "windows")]
    fn hub_exe() -> &'static Path {
        Path::new("hub.exe")
    }

    #[cfg(not(target_os = "windows"))]
    fn hub_exe() -> &'static Path {
        Path::new("hub")
    }

    /// attempt to launch instance
    /// this will use advisory file locks to make sure only one launch attempt is happening at any one time (per user).
    pub(crate) fn launch(
        &mut self,
        search_path: Option<&Path>,
        cache_path: Option<&Path>,
        cache_size: Option<&str>,
    ) -> Result<(Child, String)> {
        // make sure we have the discovery lock while searching for and launching binary
        let _ = self.discovery_lock.try_write()?;

        let mut cmd = match self.mode {
            Mode::Exclusive => Self::command_exclusive(search_path),
            Mode::Shared => Self::command_shared(search_path),
        };

        // tell binary where to write configuration
        cmd.arg("--write-config").arg(&self.config_file);

        // pass along cache directory and cache size (if specified)
        cmd.arg("--");

        if let Some(path) = cache_path {
            let mut path_arg = OsString::from("cache.path=");
            path_arg.push(path);
            cmd.arg(path_arg);
        }

        if let Some(size) = cache_size {
            cmd.arg(format!("cache.max_size={size}"));
        }

        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());

        info!(?self, ?cmd, "launch");

        // If spawning fails, then there's a problem with the executable (probably doesn't exist)
        // In that case, return HubNotFound so the client can exit immediately
        let Ok(mut child) = cmd.spawn() else {
            return Err(Error::HubNotFound);
        };
        debug!(?child);

        // wait for config file
        Self::wait_for_file(&self.config_file, &mut child, 100)?;
        let contents = fs::read_to_string(&self.config_file)?;

        // Attempt to remove the config file. Ignore failures if for some reason we can't remove.
        let _ = fs::remove_file(&self.config_file);
        debug!(contents);

        Ok((child, contents))
    }

    fn command_exclusive(search_path: Option<&Path>) -> Command {
        let path = Self::discover_binary_exclusive(search_path);
        let mut cmd = std::process::Command::new(path);

        // enable death signal so child exits with parent
        cmd.death_signal();

        // exclusive mode
        cmd.arg("--mode=exclusive");

        cmd
    }

    // discover binary in exclusive mode
    //        - search path
    //        - next to library
    //        - next to current exe
    fn discover_binary_exclusive(search_path: Option<&Path>) -> PathBuf {
        // search path is file
        if let Some(path) = search_path
            .filter(|p| p.ends_with(Self::hub_exe()) && p.is_file())
            .map(ToOwned::to_owned)
        {
            return path;
        }

        // search path is directory
        if let Some(path) = search_path.filter(|p| p.is_dir()) {
            if let Some(path) = Self::match_hub_path(path) {
                return path;
            }
        }

        // check to see if binary is next to library
        if let Some(dylib_path) = get_dylib_path() {
            if let Some(path) = dylib_path
                .parent()
                .map(|p| p.join(Self::hub_exe()))
                .filter(|p| p.is_file())
            {
                return path;
            }
        }

        // check to see if binary is next to exe
        if let Ok(exe_path) = env::current_exe() {
            if let Some(path) = exe_path
                .parent()
                .map(|p| p.join(Self::hub_exe()))
                .filter(|p| p.is_file())
            {
                return path;
            }
        }

        // fallback to empty path + filename
        PathBuf::default().join(Self::hub_exe())
    }

    fn command_shared(search_path: Option<&Path>) -> Command {
        let path = Self::discover_binary_shared(search_path);
        let mut cmd = std::process::Command::new(path);

        // shared mode
        cmd.arg("--mode=shared");

        cmd
    }

    fn discover_binary_shared(search_path: Option<&Path>) -> PathBuf {
        // search path is file
        if let Some(path) = search_path
            .filter(|p| p.ends_with(Self::hub_exe()) && p.is_file())
            .map(ToOwned::to_owned)
        {
            return path;
        }

        // search path is directory
        if let Some(path) = search_path.filter(|p| p.is_dir()) {
            if let Some(path) = Self::match_hub_path(path) {
                return path;
            }
        }

        // search in search path by semver
        if let Some(search_path) = search_path.filter(|p| p.is_dir()) {
            // directly search search path with semver
            if let Ok(Some(path)) = Self::find_latest_hub("hub", search_path, false) {
                return path;
            }
        }

        // search for omniverse config
        if let Ok(omniverse) = Settings::load_omni_config(search_path) {
            if let Ok(Some(path)) = Self::find_latest_hub("hub", omniverse.paths.library_root, true) {
                return path;
            }
        }

        // fallback to empty path + filename
        PathBuf::default().join(Self::hub_exe())
    }

    fn find_latest_hub(
        prefix: impl AsRef<str>,
        search_path: impl AsRef<Path>,
        require_install_marker: bool,
    ) -> Result<Option<PathBuf>> {
        let mut newest_version = Version::parse("0.0.0").unwrap();
        let mut newest_path: Option<PathBuf> = None;

        for entry in fs::read_dir(search_path)?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|e| e.is_dir()).unwrap_or(false))
            .filter(|e| e.file_name().to_string_lossy().starts_with(prefix.as_ref()))
        {
            if let Some((_, version_str)) = entry.file_name().to_string_lossy().split_once('-') {
                if let Ok(version) = Version::parse(version_str) {
                    if version > newest_version {
                        // HUB-906: check for the '.installed' file marker and only consider this
                        // instance if that file is present. Note that there is still a race condition
                        // upon seeing this file exists, launching hub, and launcher UI uninstalling
                        // hub, but this greatly shrinks that window. The only way to fully close the
                        // race condition would be to introduce some sort of IPC / locking between
                        // hub and the launcher UI install/uninstall process.
                        if require_install_marker && !entry.path().join(".installed").exists() {
                            continue;
                        }

                        if let Some(path) = Self::match_hub_path(entry.path()) {
                            trace!(?version, ?path, "found version");
                            newest_version = version;
                            newest_path = Some(path);
                        }
                    }
                }
            }
        }

        Ok(newest_path)
    }

    // given a base path, look for hub in some common places under that base path
    fn match_hub_path(path: impl AsRef<Path>) -> Option<PathBuf> {
        let paths = vec![
            path.as_ref().join("target/release").join(Self::hub_exe()),
            path.as_ref().join("target/debug").join(Self::hub_exe()),
            path.as_ref().join("release").join(Self::hub_exe()),
            path.as_ref().join("debug").join(Self::hub_exe()),
            path.as_ref().join(Self::hub_exe()),
        ];

        paths.into_iter().find(|p| p.is_file())
    }

    /// wait for file to exist
    fn wait_for_file(file_path: &Path, child: &mut Child, retries: u32) -> Result<()> {
        for attempt in 0..=retries {
            if file_path.exists() {
                return Ok(());
            }

            // check the status of the child, if it returned an error, we may never see the file
            match child.try_wait() {
                Ok(Some(status)) => {
                    if !status.success() {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            format!("child exited with {status} without writing file {file_path:?}"),
                        )
                        .into());
                    }
                },
                Ok(None) => {}, // child process still running
                Err(e) => return Err(e.into()),
            }

            if attempt < retries {
                debug!(?file_path, "waiting for file");
                thread::sleep(Duration::from_millis(50));
            }
        }

        Err(std::io::Error::new(std::io::ErrorKind::NotFound, format!("waited for file {file_path:?}")).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::prelude::*;
    use tempfile::tempdir;
    use tracing::trace;

    #[test_log::test]
    fn test_discover_binary_exclusive() {
        let tmp_dir = tempdir().unwrap();
        let test_binary_path = tmp_dir.path().join(Launcher::hub_exe());
        trace!(?test_binary_path, "tmp");

        File::create(&test_binary_path).unwrap();

        // test when the full path is given as search path
        assert_eq!(
            Launcher::discover_binary_exclusive(Some(&test_binary_path)),
            test_binary_path
        );

        // test when parent path is given as search path
        assert_eq!(
            Launcher::discover_binary_exclusive(Some(tmp_dir.path())),
            test_binary_path
        );
    }

    #[test_log::test]
    fn test_discover_binary_shared() {
        let tmp_dir = tempdir().unwrap();

        let test_binary_path = tmp_dir.path().join(Launcher::hub_exe());
        trace!(?test_binary_path, "tmp");
        File::create(&test_binary_path).unwrap();

        // test when the full path is given as search path
        assert_eq!(
            Launcher::discover_binary_shared(Some(&test_binary_path)),
            test_binary_path
        );

        // test when parent path is given as search path
        assert_eq!(Launcher::discover_binary_shared(Some(tmp_dir.path())), test_binary_path);

        fs::remove_file(&test_binary_path).unwrap();

        let test_dirs = vec!["hub-0.0.1", "hub-0.0.2", "hub-0.1.8", "xyz", "1234"];

        for test_dir in test_dirs {
            let path = tmp_dir.path().join(test_dir);
            fs::create_dir_all(&path).unwrap();
            if test_dir.starts_with("hub") {
                File::create(path.join(Launcher::hub_exe())).unwrap();
            }
        }

        assert_eq!(
            Launcher::discover_binary_shared(Some(tmp_dir.path())),
            tmp_dir.path().join("hub-0.1.8").join(Launcher::hub_exe())
        );

        // add newer installs and check again
        let new_version_path = tmp_dir.path().join("hub-2.8.1");
        fs::create_dir_all(new_version_path).unwrap(); // empty dir

        let new_version_path = tmp_dir.path().join("hub-2.8.2");
        fs::create_dir_all(&new_version_path).unwrap();
        File::create(new_version_path.join(Launcher::hub_exe())).unwrap();

        let new_version_path = tmp_dir.path().join("hub-2.8.3");
        fs::create_dir_all(&new_version_path).unwrap();
        File::create(new_version_path.join("not_matching_prefix_name")).unwrap();

        assert_eq!(
            Launcher::discover_binary_shared(Some(tmp_dir.path())),
            tmp_dir.path().join("hub-2.8.2").join(Launcher::hub_exe())
        );

        // remove (uninstall) and check again
        fs::remove_dir_all(tmp_dir.path().join("hub-2.8.1")).unwrap();
        fs::remove_dir_all(tmp_dir.path().join("hub-2.8.2")).unwrap();
        fs::remove_dir_all(tmp_dir.path().join("hub-2.8.3")).unwrap();

        assert_eq!(
            Launcher::discover_binary_shared(Some(tmp_dir.path())),
            tmp_dir.path().join("hub-0.1.8").join(Launcher::hub_exe())
        );
    }

    #[test_log::test]
    fn test_discover_binary_toml() {
        // HUB-941: Support searching in both the "target/release" and "release" folders
        let base_dirs = vec!["target/release", "release"];
        let test_dirs = vec!["hub-0.1.1", "hub-0.0.3", "hub-100.55.8-alpha"];

        for base_dir in &base_dirs {
            for idx in 0..test_dirs.len() {
                let tmp_dir = tempdir().unwrap();
                let test_file_name = Path::new(base_dir).join(Launcher::hub_exe());

                for test_dir in &test_dirs {
                    let path = tmp_dir.path().join(test_dir).join(&test_file_name);
                    fs::create_dir_all(path.parent().unwrap()).unwrap();
                    File::create(path).unwrap();
                }

                // install marker, created by launcher post-install script
                // only install that on one of the installs
                let install_marker_path = tmp_dir.path().join(test_dirs[idx]).join(".installed");
                File::create(install_marker_path).unwrap();

                // create dirs that either do not match the prefix or match prefix but are empty
                let bad_test_dirs = ["hub-1000.1.1", "_xyz", "1234"];

                for test_dir in bad_test_dirs {
                    let path = tmp_dir.path().join(test_dir);
                    fs::create_dir_all(&path).unwrap();
                }

                // test loading a sample omniverse.toml
                let omniverse_toml = tmp_dir.path().join("omniverse.toml");
                let mut file = File::create(&omniverse_toml).unwrap();
                writeln!(&mut file, "[paths]").unwrap();
                writeln!(&mut file, "library_root = '{}'", tmp_dir.path().to_string_lossy()).unwrap();

                let r = fs::read_to_string(&omniverse_toml).unwrap();
                trace!(?r, "omniverse.toml");

                assert_eq!(
                    Launcher::discover_binary_shared(Some(omniverse_toml.as_path())),
                    tmp_dir.path().join(test_dirs[idx]).join(test_file_name)
                );
            }
        }
    }

    #[test_log::test]
    fn test_match_hub_path() {
        let tmp_dir = tempdir().unwrap();

        let hub_path = tmp_dir.path().join(Launcher::hub_exe());
        File::create(&hub_path).unwrap();
        let result = Launcher::match_hub_path(tmp_dir.path());
        assert_eq!(result, Some(hub_path.clone()));
        fs::remove_file(hub_path).unwrap();

        let hub_path = tmp_dir.path().join("release").join(Launcher::hub_exe());
        fs::create_dir_all(hub_path.parent().unwrap()).unwrap();
        File::create(&hub_path).unwrap();
        let result = Launcher::match_hub_path(tmp_dir.path());
        assert_eq!(result, Some(hub_path.clone()));
        fs::remove_file(hub_path).unwrap();

        let hub_path = tmp_dir.path().join("debug").join(Launcher::hub_exe());
        fs::create_dir_all(hub_path.parent().unwrap()).unwrap();
        File::create(&hub_path).unwrap();
        let result = Launcher::match_hub_path(tmp_dir.path());
        assert_eq!(result, Some(hub_path.clone()));
        fs::remove_file(hub_path).unwrap();

        let hub_path = tmp_dir.path().join("target/release").join(Launcher::hub_exe());
        fs::create_dir_all(hub_path.parent().unwrap()).unwrap();
        File::create(&hub_path).unwrap();
        let result = Launcher::match_hub_path(tmp_dir.path());
        assert_eq!(result, Some(hub_path.clone()));
        fs::remove_file(hub_path).unwrap();

        let hub_path = tmp_dir.path().join("target/debug").join(Launcher::hub_exe());
        fs::create_dir_all(hub_path.parent().unwrap()).unwrap();
        File::create(&hub_path).unwrap();
        let result = Launcher::match_hub_path(tmp_dir.path());
        assert_eq!(result, Some(hub_path.clone()));
        fs::remove_file(hub_path).unwrap();

        let hub_path = tmp_dir.path().join("target").join(Launcher::hub_exe());
        fs::create_dir_all(hub_path.parent().unwrap()).unwrap();
        File::create(&hub_path).unwrap();
        let result = Launcher::match_hub_path(tmp_dir.path());
        assert!(result.is_none());
        fs::remove_file(hub_path).unwrap();

        let result = Launcher::match_hub_path(Path::new("/foo/bar/baz"));
        assert_eq!(result, None);

        let result = Launcher::match_hub_path(tempdir().unwrap().path());
        assert_eq!(result, None);
    }
}

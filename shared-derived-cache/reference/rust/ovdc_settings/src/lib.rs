// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Provides a hot-reload type settings abstraction for the store.
#![deny(unused, missing_docs)]

use std::{
    future::Future,
    io,
    ops::Deref,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use serde::de::{DeserializeOwned, Error};
use tokio::{fs, sync::watch};

use crate::value_de::ConfigValue;

pub mod value_de;

/// A generic settings store.
pub type Generic = ConfigValue;
/// A generic settings store.
pub type Cfg = Settings<Generic>;

/// Trait for reading typed values out of a settings map.
pub trait Mapping {
    /// Deserializes and returns the value at `name`.
    fn get<T: DeserializeOwned>(&self, name: &str) -> Result<T, value_de::Error>
    where
        Self: Sized;
}

impl Mapping for Generic {
    fn get<T: DeserializeOwned>(&self, name: &str) -> Result<T, value_de::Error>
    where
        Self: Sized,
    {
        let Some(table) = self.as_table() else {
            return Err(value_de::Error::custom("not a map"));
        };
        if let Some(sub_table) = table.get(name) {
            return value_de::der(sub_table);
        }

        Err(value_de::Error::custom(format!("missing {name}")))
    }
}

/// A snapshot of the settings.
///
/// This is a snapshot because while a version of settings is referenced, and
/// it is not guaranteed to be the latest version.
pub trait Snap<T> {
    /// Returns a reference to the settings.
    fn get(&self) -> &T;

    /// Updates self and brings in the latest snapshot.
    fn update(&mut self);

    /// Wait for the settings to be changed.
    fn changed(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;

    /// Returns true if this snap is outdated.
    fn is_old(&self) -> bool;

    /// Returns a version of this snapshot that can be shared between threads.
    fn shared(self) -> SharedSnap<T>
    where
        Self: Sized + Send + Sync + 'static,
    {
        SharedSnap {
            boxed: parking_lot::Mutex::new(Box::new(self)),
        }
    }

    /// Deserializes a named subsection of the settings into `S`.
    ///
    /// Panics if the subtable is missing or fails to deserialize.
    fn subtable<S: DeserializeOwned>(&self, name: &str) -> S
    where
        Self: Sized,
        T: Mapping,
    {
        self.get()
            .get(name)
            .unwrap_or_else(|e| panic!("failed to deserialize subtable {name:?}: {e}"))
    }

    /// Converts this snap into a `SubTable` scoped to `name`.
    fn into_subtable<S: DeserializeOwned>(self, name: impl Into<String>) -> SubTable<S, Self>
    where
        Self: Sized,
        T: Mapping,
    {
        SubTable {
            x: Default::default(),
            source: self,
            name: name.into(),
        }
    }
}

/// A settings snapshot that can be shared by multiple threads.
pub struct SharedSnap<T> {
    boxed: parking_lot::Mutex<Box<dyn Snap<T> + Send + Sync + 'static>>,
}

impl<T> SharedSnap<T> {
    /// Updates the snapshot and returns the most recent value.
    pub fn update_and_get(&self) -> T
    where
        T: Clone,
    {
        let mut lock = self.boxed.lock();
        lock.update();
        lock.get().clone()
    }
}

/// Loads a toml doc and modifies it.
pub fn load_document_with(
    value: &str,
    op: impl FnOnce(&mut toml_edit::DocumentMut),
) -> Settings<Generic> {
    let mut doc = value
        .parse::<toml_edit::DocumentMut>()
        .expect("failed to parse toml");
    op(&mut doc);
    Settings::new_with(toml_edit::de::from_document(doc).unwrap())
}

/// Loads a toml doc and modifies it.
pub fn load_document_to_str(value: &str, op: impl FnOnce(&mut toml_edit::DocumentMut)) -> String {
    let mut doc = value
        .parse::<toml_edit::DocumentMut>()
        .expect("failed to parse toml");
    op(&mut doc);
    doc.to_string()
}

fn load_toml<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, toml_edit::de::Error> {
    let doc = value.parse::<toml_edit::DocumentMut>()?;
    toml_edit::de::from_document(doc)
}

async fn load_with_retry(path: &Path) -> Result<String, io::Error> {
    let mut last = Result::<String, io::Error>::Err(io::ErrorKind::NotFound.into());
    for _ in 0..16 {
        last = tokio::fs::read_to_string(path).await;
        if last.is_ok() {
            return last;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    last
}

/// A check run against candidate settings before they are published.
type Validator<T> = Box<dyn Fn(&T) -> Result<(), String> + Send + Sync>;

/// Registered validators, keyed by the section name they check.
///
/// Settings are decentralized: each crate owns its own section and the type it
/// deserializes into, so there is no single place that knows every section.
/// Crates opt in via [`Settings::register_validator`] and the reload path
/// checks whatever has registered. A section with no validator keeps the
/// existing behavior of failing lazily at first read.
type Validators<T> = Arc<parking_lot::Mutex<Vec<(String, Validator<T>)>>>;

/// Runs every registered validator, collecting all failures so one restart
/// reports every bad section rather than only the first.
fn run_validators<T>(validators: &Validators<T>, settings: &T) -> Result<(), String> {
    let failures = validators
        .lock()
        .iter()
        .filter_map(|(_, check)| check(settings).err())
        .collect::<Vec<_>>();

    match failures.is_empty() {
        true => Ok(()),
        false => Err(failures.join("; ")),
    }
}

/// Settings container and provider.
pub struct Settings<T> {
    inner: Arc<watch::Sender<Arc<T>>>,
    validators: Validators<T>,
}

impl<T: serde::de::DeserializeOwned + Send + Sync + Default + 'static> Settings<T> {
    /// A [Settings] holding `value`, with no file behind it and no reload task.
    fn new_with(value: T) -> Settings<T> {
        let (send, _recv) = watch::channel(Arc::new(value));
        Settings {
            inner: Arc::new(send),
            validators: Default::default(),
        }
    }

    /// Watches a file for changes and returns a [`Settings<T>`].
    pub async fn new(path: PathBuf) -> Settings<T> {
        Self::new_polling_every(path, Duration::from_secs(5)).await
    }

    /// [`Settings::new`] with the poll interval injected, so tests can drive the
    /// reload loop without waiting on the production interval.
    async fn new_polling_every(path: PathBuf, interval: Duration) -> Settings<T> {
        // Stat before reading, and hand this baseline to the watcher. Taking it
        // afterwards loses any edit that lands in between: the baseline would
        // already carry the newer mtime while the loaded content is the older
        // one, so the change is never observed. Statting first errs the other
        // way -- at worst one spurious reload of content we already have.
        let baseline = match fs::metadata(&path).await {
            Ok(x) => x,
            Err(error_message) => {
                tracing::error!(%error_message, "failed to read settings file");
                panic!("failed to read settings file: {}", error_message);
            }
        };

        let settings_raw = match fs::read_to_string(&path).await {
            Ok(x) => x,
            Err(error_message) => {
                tracing::error!(%error_message, "could not read settings file");
                panic!("could not read settings file: {}", error_message);
            }
        };

        let settings: Arc<T> =
            Arc::new(load_toml(&settings_raw).unwrap_or_else(|e| {
                panic!("failed to parse settings file {}: {e}", path.display())
            }));

        let (send, _) = watch::channel(settings);
        let send = Arc::new(send);
        let validators: Validators<T> = Default::default();

        let sender = Arc::downgrade(&send);
        let checks = validators.clone();
        tokio::spawn(async move {
            let mut last = baseline;

            loop {
                tokio::time::sleep(interval).await;

                // Receiver count is not a shutdown signal -- a Sender can always
                // mint new Receivers. Exit only once the last Settings handle drops.
                let Some(sender) = sender.upgrade() else {
                    break;
                };

                let Ok(meta) = (match fs::metadata(&path).await {
                    Ok(x) => Ok(x),
                    Err(error_message) => {
                        tracing::error!(%error_message, "failed to read settings file");
                        Err(error_message)
                    }
                }) else {
                    continue;
                };
                let is_same = (meta.modified().ok(), meta.created().ok())
                    == (last.modified().ok(), last.created().ok());
                last = meta;

                if is_same {
                    continue;
                }

                let settings_raw = match load_with_retry(&path).await {
                    Ok(x) => x,
                    Err(error_message) => {
                        tracing::error!(%error_message, "could not read settings file");
                        panic!("could not read settings file: {}", error_message);
                    }
                };

                let settings = match load_toml(&settings_raw) {
                    Ok(x) => x,
                    Err(error_message) => {
                        tracing::error!(%error_message, "failed to load new settings");
                        panic!("could not parse toml in settings file: {}", error_message);
                    }
                };
                let settings: Arc<T> = Arc::new(settings);

                // Check registered sections before publishing, so a config edit
                // that is valid TOML but wrong for a section fails here rather
                // than at the next read of that section.
                if let Err(error_message) = run_validators(&checks, &settings) {
                    tracing::error!(%error_message, "rejected new settings");
                    panic!("invalid settings file: {error_message}");
                }

                sender.send_replace(settings);
            }
        });

        Settings {
            inner: send,
            validators,
        }
    }
}

impl<T> Clone for Settings<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            validators: self.validators.clone(),
        }
    }
}

impl<T: Mapping> Settings<T> {
    /// Registers section `name` to be checked against `S` on every reload,
    /// before the new settings are published.
    ///
    /// Call this from the crate that owns `S`, near where it first reads its
    /// section. Registering is idempotent per name, so it is safe to call from
    /// a path that runs more than once.
    ///
    /// Only registered sections are checked. An unregistered section keeps the
    /// existing behavior of failing at first read, which for a reload means
    /// failing at some later caller rather than at the edit that caused it.
    pub fn register_validator<S: serde::de::DeserializeOwned + 'static>(
        &self,
        name: impl Into<String>,
    ) {
        let name = name.into();
        let mut validators = self.validators.lock();

        if validators.iter().any(|(known, _)| known == &name) {
            return;
        }

        let section = name.clone();
        validators.push((
            name,
            Box::new(move |settings: &T| {
                settings
                    .get::<S>(&section)
                    .map(|_| ())
                    .map_err(|e| format!("[{section}]: {e}"))
            }),
        ));
    }
}

impl<T> Settings<T> {
    /// Create a snapshot of the current settings.
    pub fn snap(&self) -> Snapped<T> {
        // Subscribe first, then take the value through the receiver, so the
        // snapshot and its seen-version marker come from a single borrow. Doing
        // these as two separate reads lets an update land in between, producing
        // a stale snapshot whose `is_old()` reports false.
        let mut recv = self.inner.subscribe();
        let inner = recv.borrow_and_update().clone();
        Snapped { inner, recv }
    }
}

/// A snapshot of [Settings].
///
/// This struct implements [Snap], which can be used to access settings.
pub struct Snapped<T> {
    inner: Arc<T>,
    recv: watch::Receiver<Arc<T>>,
}

impl<T: 'static + Send + Sync> Snap<T> for Snapped<T> {
    fn get(&self) -> &T {
        &self.inner
    }

    fn update(&mut self) {
        self.inner = self.recv.borrow_and_update().clone();
    }

    fn changed(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let mut recv = self.recv.clone();
        Box::pin(async move {
            let _ = recv.changed().await;
        })
    }

    fn is_old(&self) -> bool {
        self.recv.has_changed().unwrap_or_default()
    }
}

impl<T: 'static + Send + Sync> Deref for Snapped<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.get()
    }
}

/// A helper that can be used to provide a new [Snap] that returns only a subset
/// or scope of settings.
/// This allows crates to encapsulate their settings and not dig through a
/// global configuration for the subset they are looking for.
pub struct SubTable<T, S> {
    x: std::sync::OnceLock<T>,
    source: S,
    name: String,
}

impl<T, S> Snap<T> for SubTable<T, S>
where
    T: serde::de::DeserializeOwned + Send + Sync,
    S: Snap<Generic>,
{
    fn get(&self) -> &T {
        self.x.get_or_init(|| {
            self.source
                .get()
                .get(&self.name)
                .unwrap_or_else(|e| panic!("failed to deserialize subtable {:?}: {e}", self.name))
        })
    }

    fn update(&mut self) {
        self.x.take();
        self.source.update()
    }

    fn changed(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.source.changed()
    }

    fn is_old(&self) -> bool {
        self.source.is_old()
    }
}

#[cfg(test)]
mod reload_tests {
    use super::*;
    use std::io::Write;
    use std::path::Path;

    #[derive(serde::Deserialize)]
    struct Section {
        count: u32,
    }

    fn write(path: &Path, body: &str) {
        let mut file = std::fs::File::create(path).unwrap();
        file.write_all(body.as_bytes()).unwrap();
        file.sync_all().unwrap();
    }

    fn count_of(settings: &Cfg) -> Option<u32> {
        let snap = settings.snap();
        snap.get().get::<Section>("s").ok().map(|s| s.count)
    }

    /// The watcher does real filesystem work on the blocking pool, which a
    /// paused clock cannot advance through -- so these tests run on real time
    /// with a short injected poll interval instead.
    const POLL: Duration = Duration::from_millis(20);

    /// Polls `check` until it holds or the budget runs out.
    async fn eventually(mut check: impl FnMut() -> bool) -> bool {
        for _ in 0..250 {
            if check() {
                return true;
            }
            tokio::time::sleep(POLL).await;
        }
        false
    }

    /// Waits out several poll cycles, for asserting that nothing happened.
    async fn settle() {
        tokio::time::sleep(POLL * 10).await;
    }

    #[tokio::test]
    async fn republishes_when_the_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write(&path, "[s]\ncount = 1\n");

        let settings: Cfg = Settings::new_polling_every(path.clone(), POLL).await;
        assert_eq!(count_of(&settings), Some(1));

        write(&path, "[s]\ncount = 2\n");
        assert!(
            eventually(|| count_of(&settings) == Some(2)).await,
            "watcher never picked up the changed file"
        );
    }

    #[tokio::test]
    async fn does_not_republish_an_unchanged_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write(&path, "[s]\ncount = 1\n");

        let settings: Cfg = Settings::new_polling_every(path.clone(), POLL).await;
        let mut snap = settings.snap();

        // Let several poll cycles pass without touching the file.
        settle().await;

        assert!(!snap.is_old(), "unchanged file should not mark snaps stale");
        snap.update();
        assert_eq!(snap.get().get::<Section>("s").unwrap().count, 1);
    }

    #[tokio::test]
    async fn runs_registered_validators_before_publishing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write(&path, "[s]\ncount = 1\n");

        let settings: Cfg = Settings::new_polling_every(path.clone(), POLL).await;
        settings.register_validator::<Section>("s");

        // A valid edit still publishes with a validator registered.
        write(&path, "[s]\ncount = 2\n");
        assert!(
            eventually(|| count_of(&settings) == Some(2)).await,
            "a valid edit should still publish when a validator is registered"
        );
    }

    #[tokio::test]
    async fn does_not_publish_a_config_that_fails_its_validator() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write(&path, "[s]\ncount = 1\n");

        let settings: Cfg = Settings::new_polling_every(path.clone(), POLL).await;
        settings.register_validator::<Section>("s");

        // Valid TOML, wrong shape for the section. The watcher rejects it by
        // panicking (fail-fast is deliberate -- see the reload loop), which
        // kills only the watcher task under a test's unwind. What matters here
        // is that the bad value never reached readers. The panic backtrace in
        // the test output is expected.
        write(&path, "[s]\ncount = \"not a number\"\n");
        settle().await;

        assert_eq!(
            count_of(&settings),
            Some(1),
            "last known-good settings should still be served"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize)]
    struct Section {
        #[allow(dead_code)]
        count: u32,
    }

    fn cfg(toml: &str) -> Cfg {
        load_document_with(toml, |_| {})
    }

    #[test]
    fn accepts_a_section_that_matches() {
        let settings = cfg("[store]\ncount = 1\n");
        settings.register_validator::<Section>("store");

        let snap = settings.snap();
        assert!(run_validators(&settings.validators, snap.get()).is_ok());
    }

    #[test]
    fn rejects_a_section_with_a_wrong_field_type() {
        let settings = cfg("[store]\ncount = \"not a number\"\n");
        settings.register_validator::<Section>("store");

        let snap = settings.snap();
        let error = run_validators(&settings.validators, snap.get()).unwrap_err();
        assert!(
            error.contains("[store]"),
            "error should name the section: {error}"
        );
    }

    #[test]
    fn rejects_a_missing_section() {
        let settings = cfg("[other]\ncount = 1\n");
        settings.register_validator::<Section>("store");

        let snap = settings.snap();
        assert!(run_validators(&settings.validators, snap.get()).is_err());
    }

    #[test]
    fn unregistered_sections_are_not_checked() {
        let settings = cfg("[store]\ncount = \"not a number\"\n");

        let snap = settings.snap();
        assert!(run_validators(&settings.validators, snap.get()).is_ok());
    }

    #[test]
    fn registering_the_same_section_twice_is_idempotent() {
        let settings = cfg("[store]\ncount = 1\n");
        settings.register_validator::<Section>("store");
        settings.register_validator::<Section>("store");

        assert_eq!(settings.validators.lock().len(), 1);
    }

    #[derive(serde::Deserialize)]
    struct Mixed {
        name: String,
        size: u64,
    }

    fn mixed(toml: &str) -> Mixed {
        let settings = cfg(toml);
        let snap = settings.snap();
        snap.get().get("s").unwrap()
    }

    #[test]
    fn string_fields_round_trip_verbatim() {
        // Each of these parses as a human number, so eager coercion would
        // rewrite them: "64M" -> 64000000, "10s" -> 10000, "007" -> 7.
        for value in ["64M", "10s", "007", "1"] {
            let section = mixed(&format!("[s]\nname = \"{value}\"\nsize = 1\n"));
            assert_eq!(section.name, value);
        }
    }

    #[test]
    fn numeric_fields_still_accept_human_sizes() {
        assert_eq!(
            mixed("[s]\nname = \"x\"\nsize = \"32G\"\n").size,
            32_000_000_000
        );
        assert_eq!(
            mixed("[s]\nname = \"x\"\nsize = \"64M\"\n").size,
            64_000_000
        );
    }

    #[test]
    fn numeric_fields_still_accept_human_durations() {
        assert_eq!(mixed("[s]\nname = \"x\"\nsize = \"10s\"\n").size, 10_000);
    }

    #[test]
    fn numeric_fields_still_accept_plain_integers() {
        assert_eq!(mixed("[s]\nname = \"x\"\nsize = 4096\n").size, 4096);
    }

    #[test]
    fn lowercase_m_is_minutes_not_megabytes() {
        // Case is significant: "m" is a time unit, "M" is a size unit. A numeric
        // field given "64m" gets 64 minutes in milliseconds, not 64 megabytes.
        assert_eq!(mixed("[s]\nname = \"x\"\nsize = \"64m\"\n").size, 3_840_000);
        assert_eq!(
            mixed("[s]\nname = \"x\"\nsize = \"64M\"\n").size,
            64_000_000
        );

        // Same on a field explicitly typed as a size -- the interpretation of
        // the unit does not depend on the target type.
        #[derive(serde::Deserialize)]
        struct Sized {
            size: ovdc_bytes::human_serde::HumanByteSize,
        }
        let typed = |toml: &str| -> u64 {
            let settings = cfg(toml);
            let snap = settings.snap();
            *snap.get().get::<Sized>("s").unwrap().size
        };
        assert_eq!(typed("[s]\nsize = \"64m\"\n"), 3_840_000);
        assert_eq!(typed("[s]\nsize = \"64M\"\n"), 64_000_000);
    }

    #[test]
    fn deny_unknown_fields_is_honored_by_this_deserializer() {
        // Config structs rely on `deny_unknown_fields` to catch a misspelled
        // key that would otherwise be discarded while the real field silently
        // takes its default. That only works if this hand-written deserializer
        // surfaces unexpected keys to serde, so assert it rather than assume.
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        #[allow(dead_code)]
        struct Strict {
            count: u32,
        }

        let settings = cfg("[s]\ncount = 1\ncuont = 2\n");
        let snap = settings.snap();

        match snap.get().get::<Strict>("s") {
            Err(error) => assert!(
                error.to_string().contains("cuont"),
                "error should name the unknown key, got: {error}"
            ),
            Ok(_) => panic!("unknown key was silently accepted"),
        }
    }

    #[test]
    fn errors_name_the_path_to_the_failing_value() {
        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
        struct Inner {
            count: u32,
        }
        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
        struct Outer {
            inner: Inner,
        }

        let settings = cfg("[s.inner]\ncount = \"not a number\"\n");
        let snap = settings.snap();
        let error = match snap.get().get::<Outer>("s") {
            Err(error) => error.to_string(),
            Ok(_) => panic!("expected a deserialization failure"),
        };

        assert!(
            error.starts_with("inner.count: "),
            "error should lead with the key path, got: {error}"
        );
    }

    #[test]
    fn every_failing_section_is_reported_together() {
        let settings = cfg("[a]\ncount = \"x\"\n\n[b]\ncount = \"y\"\n");
        settings.register_validator::<Section>("a");
        settings.register_validator::<Section>("b");

        let snap = settings.snap();
        let error = run_validators(&settings.validators, snap.get()).unwrap_err();
        assert!(error.contains("[a]") && error.contains("[b]"), "{error}");
    }
}

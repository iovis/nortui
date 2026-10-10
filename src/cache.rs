use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::model::Location;

const VERSION: u32 = 1;
const WRITE_DELAY: Duration = Duration::from_millis(250);
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Catalog {
    pub version: u32,
    pub updated_at: u64,
    pub countries: BTreeMap<String, Vec<String>>,
}

impl Default for Catalog {
    fn default() -> Self {
        Self {
            version: VERSION,
            updated_at: 0,
            countries: BTreeMap::new(),
        }
    }
}

impl Catalog {
    pub fn load(path: &Path) -> Result<Self> {
        let data = match fs::read(path) {
            Ok(data) => data,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error).context("Cannot read location cache"),
        };
        let catalog: Self = serde_json::from_slice(&data).context("Invalid location cache")?;
        if catalog.version != VERSION {
            bail!("Unsupported location cache version");
        }
        if catalog.countries.iter().any(|(country, cities)| {
            !valid_token(country) || cities.iter().any(|city| !valid_token(city))
        }) {
            bail!("Invalid location names in cache");
        }
        Ok(catalog)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path.parent().context("Cache path has no parent")?;
        fs::create_dir_all(parent).context("Cannot create location cache directory")?;
        let temporary = parent.join(format!(
            ".locations-{}-{}.tmp",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let result: Result<()> = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&serde_json::to_vec(self)?)?;
            file.sync_all()?;
            fs::rename(&temporary, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.context("Cannot save location cache")
    }

    pub fn set_countries(&mut self, countries: &[String]) -> bool {
        let countries: BTreeSet<_> = countries.iter().map(String::as_str).collect();
        let unchanged = self.countries.len() == countries.len()
            && countries
                .iter()
                .all(|country| self.countries.contains_key(*country));
        if unchanged {
            return false;
        }
        self.countries
            .retain(|country, _| countries.contains(country.as_str()));
        for country in countries {
            self.countries.entry(country.to_owned()).or_default();
        }
        self.touch();
        true
    }

    pub fn set_cities(&mut self, country: String, mut cities: Vec<String>) -> bool {
        cities.sort();
        cities.dedup();
        if self.countries.get(&country) == Some(&cities) {
            return false;
        }
        self.countries.insert(country, cities);
        self.touch();
        true
    }

    pub fn locations(&self) -> Vec<Location> {
        self.countries
            .iter()
            .flat_map(|(country, cities)| {
                std::iter::once(Location {
                    country: country.clone(),
                    city: None,
                })
                .chain(cities.iter().map(|city| Location {
                    country: country.clone(),
                    city: Some(city.clone()),
                }))
            })
            .collect()
    }

    fn touch(&mut self) {
        self.updated_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
    }
}

#[derive(Default)]
struct WriteState {
    pending: Option<Catalog>,
    in_flight: Option<Catalog>,
    saved: Option<Catalog>,
    flush: bool,
    closing: bool,
}

pub struct Writer {
    state: Arc<Mutex<WriteState>>,
    changed: Arc<Notify>,
    results: mpsc::Receiver<Result<(), String>>,
    job: Option<JoinHandle<()>>,
}

impl Writer {
    pub fn new(path: PathBuf, handle: &Handle) -> Self {
        Self::start(path, handle, Catalog::save)
    }

    fn start(
        path: PathBuf,
        handle: &Handle,
        save: impl Fn(&Catalog, &Path) -> Result<()> + Send + Sync + 'static,
    ) -> Self {
        let state = Arc::new(Mutex::new(WriteState::default()));
        let changed = Arc::new(Notify::new());
        let (sender, results) = mpsc::channel();
        let job = handle.spawn(write_catalogs(
            path,
            Arc::clone(&state),
            Arc::clone(&changed),
            sender,
            Arc::new(save),
        ));
        Self {
            state,
            changed,
            results,
            job: Some(job),
        }
    }

    pub fn submit(&mut self, catalog: Catalog) {
        let mut state = self.state.lock().expect("Cache writer lock poisoned");
        if state.closing || same_data(state.pending.as_ref(), &catalog) {
            return;
        }
        if same_data(state.in_flight.as_ref(), &catalog)
            || (state.in_flight.is_none() && same_data(state.saved.as_ref(), &catalog))
        {
            // A newer submission can return to the catalog already being saved.
            state.pending = None;
        } else {
            state.pending = Some(catalog);
        }
        drop(state);
        self.changed.notify_one();
    }

    pub fn flush(&self) {
        let mut state = self.state.lock().expect("Cache writer lock poisoned");
        state.flush = state.pending.is_some();
        drop(state);
        self.changed.notify_one();
    }

    pub fn poll(&mut self) -> Option<Result<(), String>> {
        self.results.try_recv().ok()
    }

    pub async fn shutdown(&mut self) {
        self.close();
        if let Some(job) = self.job.take()
            && let Err(error) = job.await
        {
            // Preserve worker failures for the same result path as save errors.
            let (sender, results) = mpsc::channel();
            let _ = sender.send(Err(format!("Cache writer failed: {error}")));
            self.results = results;
        }
    }

    fn close(&self) {
        let mut state = self.state.lock().expect("Cache writer lock poisoned");
        state.closing = true;
        state.flush = true;
        drop(state);
        self.changed.notify_one();
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.close();
    }
}

fn same_data(previous: Option<&Catalog>, catalog: &Catalog) -> bool {
    previous.is_some_and(|previous| {
        previous.version == catalog.version && previous.countries == catalog.countries
    })
}

async fn write_catalogs(
    path: PathBuf,
    state: Arc<Mutex<WriteState>>,
    changed: Arc<Notify>,
    results: mpsc::Sender<Result<(), String>>,
    save: Arc<impl Fn(&Catalog, &Path) -> Result<()> + Send + Sync + 'static>,
) {
    let mut deadline = None;
    loop {
        let notified = changed.notified();
        let catalog = {
            let mut state = state.lock().expect("Cache writer lock poisoned");
            if state.pending.is_none() {
                deadline = None;
                state.flush = false;
                if state.closing {
                    return;
                }
                None
            } else if state.closing
                || state.flush
                || deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
            {
                let catalog = state.pending.take();
                state.in_flight.clone_from(&catalog);
                state.flush = false;
                catalog
            } else {
                deadline.get_or_insert_with(|| tokio::time::Instant::now() + WRITE_DELAY);
                None
            }
        };
        if let Some(catalog) = catalog {
            let snapshot = catalog.clone();
            let path = path.clone();
            let save = Arc::clone(&save);
            let result = tokio::task::spawn_blocking(move || save(&snapshot, &path))
                .await
                .map_err(|error| format!("Cache writer failed: {error}"))
                .and_then(|result| result.map_err(|error| format!("{error:#}")));
            {
                let mut state = state.lock().expect("Cache writer lock poisoned");
                state.in_flight = None;
                if result.is_ok() {
                    state.saved = Some(catalog);
                }
            }
            let _ = results.send(result);
            deadline = None;
        } else if let Some(deadline) = deadline {
            tokio::select! {
                () = notified => {},
                () = tokio::time::sleep_until(deadline) => {},
            }
        } else {
            notified.await;
        }
    }
}

pub fn default_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .map(|root| root.join("nortui/locations.json"))
}

fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_round_trip_and_failed_country_refresh_preserve_cities() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("cache/locations.json");
        let mut catalog = Catalog::default();
        catalog.set_cities("United_States".into(), vec!["New_York".into()]);
        catalog.save(&path)?;
        catalog.set_countries(&["United_States".into(), "Japan".into()]);
        catalog.save(&path)?;
        let loaded = Catalog::load(&path)?;
        assert_eq!(loaded.countries["United_States"], ["New_York"]);
        assert_eq!(loaded.locations().len(), 3);
        assert_eq!(fs::read_dir(path.parent().unwrap())?.count(), 1);
        Ok(())
    }

    #[test]
    fn missing_corrupt_and_unsupported_caches_are_recoverable() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("locations.json");
        assert!(Catalog::load(&path)?.countries.is_empty());
        fs::write(&path, b"partial json")?;
        assert!(Catalog::load(&path).is_err());
        let mut catalog = Catalog::default();
        catalog.save(&path)?;
        assert!(Catalog::load(&path).is_ok());
        catalog.version = 99;
        catalog.save(&path)?;
        assert!(Catalog::load(&path).is_err());
        Ok(())
    }

    #[test]
    fn unchanged_catalog_data_preserves_timestamp_and_cities() {
        let mut catalog = Catalog::default();
        assert!(catalog.set_cities(
            "United_States".into(),
            vec!["Seattle".into(), "New_York".into(), "Seattle".into()]
        ));
        catalog.updated_at = 123;
        assert!(!catalog.set_cities(
            "United_States".into(),
            vec!["New_York".into(), "Seattle".into()]
        ));
        assert!(!catalog.set_countries(&["United_States".into(), "United_States".into()]));
        assert_eq!(catalog.updated_at, 123);
        assert_eq!(catalog.countries["United_States"], ["New_York", "Seattle"]);

        assert!(catalog.set_countries(&["Japan".into(), "United_States".into()]));
        assert_eq!(catalog.countries["United_States"], ["New_York", "Seattle"]);
        assert_eq!(catalog.countries["Japan"], Vec::<String>::new());
        assert!(catalog.set_countries(&["Japan".into()]));
        assert!(!catalog.countries.contains_key("United_States"));
    }

    fn test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    async fn next_result(writer: &mut Writer) -> Result<(), String> {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(result) = writer.poll() {
                    return result;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("Cache write did not finish")
    }

    #[test]
    fn writer_batches_changes_and_skips_timestamp_only_submissions() -> Result<()> {
        let runtime = test_runtime();
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("locations.json");
        let mut writer = Writer::new(path.clone(), runtime.handle());
        runtime.block_on(async {
            let mut catalog = Catalog::default();
            for country in ["Japan", "United_States", "France"] {
                catalog.set_cities(country.into(), vec!["City".into()]);
                writer.submit(catalog.clone());
                tokio::task::yield_now().await;
            }
            assert!(!path.exists(), "Batch was saved before its delay expired");
            next_result(&mut writer).await.unwrap();
            assert_eq!(Catalog::load(&path).unwrap().countries.len(), 3);
            assert!(writer.poll().is_none(), "Batch caused multiple writes");

            // A successful save must not be repeated even if its timestamp changes.
            let saved_data = fs::read(&path).unwrap();
            catalog.updated_at += 1;
            writer.submit(catalog);
            writer.shutdown().await;
            assert_eq!(fs::read(&path).unwrap(), saved_data);
            assert!(writer.poll().is_none());
        });
        Ok(())
    }

    #[test]
    fn writer_serializes_saves_and_shutdown_flushes_the_latest_snapshot() -> Result<()> {
        let runtime = test_runtime();
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("locations.json");
        let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
        let (release, gate) = mpsc::channel();
        let gate = Mutex::new(gate);
        let mut writer = Writer::start(path.clone(), runtime.handle(), move |catalog, path| {
            started.send(catalog.countries.len()).unwrap();
            gate.lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
            catalog.save(path)
        });
        runtime.block_on(async {
            let mut catalog = Catalog::default();
            catalog.set_countries(&["Japan".into()]);
            writer.submit(catalog.clone());
            writer.flush();
            assert_eq!(starts.recv().await, Some(1));

            catalog.set_countries(&["Japan".into(), "France".into()]);
            writer.submit(catalog.clone());
            catalog.set_countries(&["Japan".into(), "France".into(), "United_States".into()]);
            writer.submit(catalog);
            {
                let shutdown = writer.shutdown();
                tokio::pin!(shutdown);
                assert!(
                    tokio::time::timeout(Duration::from_millis(20), shutdown.as_mut())
                        .await
                        .is_err(),
                    "Shutdown returned while a save was in flight"
                );
                assert!(starts.try_recv().is_err(), "Cache saves overlapped");
                release.send(()).unwrap();
                assert_eq!(starts.recv().await, Some(3));
                assert!(
                    tokio::time::timeout(Duration::from_millis(20), shutdown.as_mut())
                        .await
                        .is_err(),
                    "Shutdown returned before the latest save completed"
                );
                release.send(()).unwrap();
                shutdown.await;
            }
            assert_eq!(Catalog::load(&path).unwrap().countries.len(), 3);
            assert!(writer.poll().unwrap().is_ok());
            assert!(writer.poll().unwrap().is_ok());
            assert!(writer.poll().is_none(), "Superseded snapshot was saved");
        });
        Ok(())
    }

    #[test]
    fn writer_reports_failed_saves_and_allows_the_same_snapshot_to_retry() -> Result<()> {
        let runtime = test_runtime();
        let directory = tempfile::tempdir()?;
        let parent = directory.path().join("blocked");
        fs::write(&parent, b"not a directory")?;
        let path = parent.join("locations.json");
        let mut writer = Writer::new(path.clone(), runtime.handle());
        runtime.block_on(async {
            let mut catalog = Catalog::default();
            catalog.set_countries(&["Japan".into()]);
            writer.submit(catalog.clone());
            writer.flush();
            let error = next_result(&mut writer).await.unwrap_err();
            assert!(error.contains("Cannot create location cache directory"));

            fs::remove_file(&parent).unwrap();
            writer.submit(catalog);
            writer.shutdown().await;
            assert!(writer.poll().unwrap().is_ok());
            assert!(
                Catalog::load(&path)
                    .unwrap()
                    .countries
                    .contains_key("Japan")
            );
        });
        Ok(())
    }
}

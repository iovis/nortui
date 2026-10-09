use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::model::Location;

const VERSION: u32 = 1;
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

    pub fn set_countries(&mut self, countries: &[String]) {
        self.countries
            .retain(|country, _| countries.contains(country));
        for country in countries {
            self.countries.entry(country.clone()).or_default();
        }
        self.touch();
    }

    pub fn set_cities(&mut self, country: String, mut cities: Vec<String>) {
        cities.sort();
        cities.dedup();
        self.countries.insert(country, cities);
        self.touch();
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
}

use crate::{ConnectionColor, ConnectionConfig, normalize_project_roots};
use anyhow::{Context as _, Result, anyhow};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

static PROFILE_STORE_LOCK: Mutex<()> = Mutex::new(());

pub fn load_profiles(path: &Path) -> Result<Vec<ConnectionConfig>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("Invalid database connection profiles"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).context("Could not read database connection profiles"),
    }
}

pub fn save_profile(path: &Path, config: &ConnectionConfig) -> Result<()> {
    let config = normalized_config(config)?;
    modify_profiles(path, move |profiles| {
        if let Some(existing) = profiles.iter_mut().find(|profile| profile.id == config.id) {
            *existing = config;
        } else {
            profiles.push(config);
        }
        Ok(true)
    })
}

pub fn save_profile_if_unchanged(
    path: &Path,
    config: &ConnectionConfig,
    expected: Option<&ConnectionConfig>,
) -> Result<()> {
    let config = normalized_config(config)?;
    modify_profiles(path, |profiles| {
        let existing = profiles.iter_mut().find(|profile| profile.id == config.id);
        match (expected, existing) {
            (None, None) => profiles.push(config),
            (Some(expected), Some(existing)) if *existing == *expected => *existing = config,
            _ => anyhow::bail!(
                "Connection changed or was removed since this editor was opened; reopen its settings before saving"
            ),
        }
        Ok(true)
    })
}

fn normalized_config(config: &ConnectionConfig) -> Result<ConnectionConfig> {
    let mut config = config.clone();
    if let Some(scope) = &config.project_scope {
        config.project_scope = Some(normalize_project_roots(scope)?);
    }
    Ok(config)
}

pub fn remove_profile(path: &Path, id: &str) -> Result<()> {
    modify_profiles(path, |profiles| {
        let count = profiles.len();
        profiles.retain(|profile| profile.id != id);
        Ok(profiles.len() != count)
    })
}

pub fn update_profile_metadata(
    path: &Path,
    id: &str,
    color: Option<ConnectionColor>,
    project_scope: Option<Vec<PathBuf>>,
) -> Result<()> {
    let project_scope = project_scope
        .as_deref()
        .map(normalize_project_roots)
        .transpose()?;
    modify_profile(path, id, |profile| {
        profile.color = color;
        profile.project_scope = project_scope;
    })
}

pub fn update_profile_color(path: &Path, id: &str, color: Option<ConnectionColor>) -> Result<()> {
    modify_profile(path, id, |profile| profile.color = color)
}

pub fn update_profile_scope(
    path: &Path,
    id: &str,
    project_scope: Option<Vec<PathBuf>>,
) -> Result<()> {
    let project_scope = project_scope
        .as_deref()
        .map(normalize_project_roots)
        .transpose()?;
    modify_profile(path, id, |profile| profile.project_scope = project_scope)
}

fn modify_profile(path: &Path, id: &str, update: impl FnOnce(&mut ConnectionConfig)) -> Result<()> {
    modify_profiles(path, |profiles| {
        let profile = profiles
            .iter_mut()
            .find(|profile| profile.id == id)
            .context("Connection profile no longer exists; reload the Databases panel")?;
        update(profile);
        Ok(true)
    })
}

fn modify_profiles(
    path: &Path,
    update: impl FnOnce(&mut Vec<ConnectionConfig>) -> Result<bool>,
) -> Result<()> {
    // Multiple Zed windows can save independently. Merge with the current file
    // under one lock instead of overwriting another window's newer profiles.
    let _guard = PROFILE_STORE_LOCK
        .lock()
        .map_err(|_| anyhow!("Database connection profile store is unavailable"))?;
    let mut profiles = load_profiles(path)?;
    if !update(&mut profiles)? {
        return Ok(());
    }
    write_profiles(path, &profiles)
}

fn write_profiles(path: &Path, profiles: &[ConnectionConfig]) -> Result<()> {
    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(directory).context("Could not create database connection directory")?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)
        .context("Could not create temporary database connection file")?;
    serde_json::to_writer_pretty(&mut temporary, profiles)
        .context("Could not serialize database connection profiles")?;
    temporary
        .as_file()
        .sync_all()
        .context("Could not write database connection profiles")?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .context("Could not replace database connection profiles")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn concurrent_saves_preserve_both_new_profiles() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("profiles.json");
        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for name in ["first", "second"] {
            let barrier = barrier.clone();
            let path = path.clone();
            workers.push(std::thread::spawn(move || {
                let config = ConnectionConfig {
                    name: name.into(),
                    ..Default::default()
                };
                barrier.wait();
                save_profile(&path, &config)
            }));
        }
        barrier.wait();
        for worker in workers {
            worker
                .join()
                .map_err(|_| anyhow!("Profile writer panicked"))??;
        }
        let mut names = load_profiles(&path)?
            .into_iter()
            .map(|config| config.name)
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, ["first", "second"]);
        Ok(())
    }

    #[test]
    fn updating_a_profile_preserves_other_profiles() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("profiles.json");
        let mut first = ConnectionConfig {
            name: "first".into(),
            ..Default::default()
        };
        let second = ConnectionConfig {
            name: "second".into(),
            ..Default::default()
        };
        save_profile(&path, &first)?;
        save_profile(&path, &second)?;
        first.name = "renamed".into();
        save_profile(&path, &first)?;
        let profiles = load_profiles(&path)?;
        assert_eq!(profiles.len(), 2);
        assert!(
            profiles
                .iter()
                .any(|profile| profile.id == first.id && profile.name == "renamed")
        );
        assert!(
            profiles
                .iter()
                .any(|profile| profile.id == second.id && profile.name == "second")
        );
        Ok(())
    }

    #[test]
    fn invalid_json_is_never_overwritten_by_a_save() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("profiles.json");
        let invalid = b"{broken json";
        std::fs::write(&path, invalid)?;
        assert!(save_profile(&path, &ConnectionConfig::default()).is_err());
        assert!(remove_profile(&path, "any-profile").is_err());
        assert_eq!(std::fs::read(&path)?, invalid);
        Ok(())
    }

    #[test]
    fn remove_and_concurrent_save_preserve_unrelated_profiles() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("profiles.json");
        let removed = ConnectionConfig {
            name: "removed".into(),
            ..Default::default()
        };
        let kept = ConnectionConfig {
            name: "kept".into(),
            ..Default::default()
        };
        let added = ConnectionConfig {
            name: "added".into(),
            ..Default::default()
        };
        save_profile(&path, &removed)?;
        save_profile(&path, &kept)?;
        let barrier = Arc::new(Barrier::new(2));
        let remover = std::thread::spawn({
            let path = path.clone();
            let barrier = barrier.clone();
            move || {
                barrier.wait();
                remove_profile(&path, &removed.id)
            }
        });
        barrier.wait();
        save_profile(&path, &added)?;
        remover
            .join()
            .map_err(|_| anyhow!("Profile remover panicked"))??;
        let mut profiles = load_profiles(&path)?;
        profiles.sort_by(|left, right| left.name.cmp(&right.name));
        assert_eq!(
            profiles
                .iter()
                .map(|profile| profile.name.as_str())
                .collect::<Vec<_>>(),
            ["added", "kept"]
        );
        assert!(profiles.iter().any(|profile| profile.id == kept.id));
        let before = std::fs::read(&path)?;
        remove_profile(&path, "missing-id")?;
        assert_eq!(std::fs::read(&path)?, before);
        Ok(())
    }

    #[test]
    fn saving_scope_normalizes_and_invalid_scope_does_not_overwrite() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("profiles.json");
        let mut config = ConnectionConfig {
            project_scope: Some(vec![
                Path::new("/z/../b").into(),
                Path::new("/a").into(),
                Path::new("/b").into(),
            ]),
            ..Default::default()
        };
        save_profile(&path, &config)?;
        let saved = load_profiles(&path)?;
        assert_eq!(
            saved
                .first()
                .context("Missing saved profile")?
                .project_scope,
            Some(vec![Path::new("/a").into(), Path::new("/b").into()])
        );
        let before = std::fs::read(&path)?;
        config.project_scope = Some(vec![Path::new("relative").into()]);
        assert!(save_profile(&path, &config).is_err());
        assert_eq!(std::fs::read(&path)?, before);
        Ok(())
    }

    #[test]
    fn independent_metadata_changes_merge_and_refuse_removed_profiles() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("profiles.json");
        let config = ConnectionConfig::default();
        save_profile(&path, &config)?;
        let barrier = Arc::new(Barrier::new(2));
        let color_worker = std::thread::spawn({
            let barrier = barrier.clone();
            let path = path.clone();
            let id = config.id.clone();
            move || {
                barrier.wait();
                update_profile_color(&path, &id, Some(ConnectionColor::Red))
            }
        });
        barrier.wait();
        update_profile_scope(&path, &config.id, Some(vec![PathBuf::from("/project")]))?;
        color_worker
            .join()
            .map_err(|_| anyhow!("Color writer panicked"))??;
        let saved = load_profiles(&path)?
            .into_iter()
            .next()
            .context("Missing profile")?;
        assert_eq!(saved.color, Some(ConnectionColor::Red));
        assert_eq!(saved.project_scope, Some(vec![PathBuf::from("/project")]));
        remove_profile(&path, &config.id)?;
        assert!(update_profile_color(&path, &config.id, None).is_err());
        assert!(update_profile_scope(&path, &config.id, None).is_err());
        assert!(load_profiles(&path)?.is_empty());
        Ok(())
    }

    #[test]
    fn stale_editor_cannot_overwrite_or_resurrect_a_profile() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("profiles.json");
        let original = ConnectionConfig::default();
        save_profile_if_unchanged(&path, &original, None)?;
        assert!(save_profile_if_unchanged(&path, &original, None).is_err());
        let mut edited = original.clone();
        edited.host = "edited.example.test".into();
        update_profile_color(&path, &original.id, Some(ConnectionColor::Blue))?;
        let before = std::fs::read(&path)?;
        assert!(save_profile_if_unchanged(&path, &edited, Some(&original)).is_err());
        assert_eq!(std::fs::read(&path)?, before);
        remove_profile(&path, &original.id)?;
        assert!(save_profile_if_unchanged(&path, &edited, Some(&original)).is_err());
        assert!(load_profiles(&path)?.is_empty());
        Ok(())
    }

    #[test]
    fn concurrent_editors_cannot_overwrite_each_other() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("profiles.json");
        let original = ConnectionConfig::default();
        save_profile_if_unchanged(&path, &original, None)?;
        let barrier = Arc::new(Barrier::new(2));
        let mut workers = Vec::new();
        for host in ["first.example.test", "second.example.test"] {
            let original = original.clone();
            let path = path.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                let mut edited = original.clone();
                edited.host = host.into();
                barrier.wait();
                save_profile_if_unchanged(&path, &edited, Some(&original)).is_ok()
            }));
        }
        let mut saved_count = 0;
        for worker in workers {
            if worker
                .join()
                .map_err(|_| anyhow!("Profile editor panicked"))?
            {
                saved_count += 1;
            }
        }
        assert_eq!(saved_count, 1);
        assert_eq!(load_profiles(&path)?.len(), 1);
        Ok(())
    }

    #[test]
    fn metadata_update_preserves_current_endpoint_and_never_resurrects_removed_profile()
    -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("profiles.json");
        let mut config = ConnectionConfig::default();
        save_profile(&path, &config)?;
        config.host = "changed.example.test".into();
        config.port = 15432;
        save_profile(&path, &config)?;
        update_profile_metadata(
            &path,
            &config.id,
            Some(ConnectionColor::Purple),
            Some(vec![PathBuf::from("/projects/z/../a")]),
        )?;
        let updated = load_profiles(&path)?
            .into_iter()
            .next()
            .context("Missing profile")?;
        assert_eq!(updated.host, "changed.example.test");
        assert_eq!(updated.port, 15432);
        assert_eq!(updated.color, Some(ConnectionColor::Purple));
        assert_eq!(
            updated.project_scope,
            Some(vec![PathBuf::from("/projects/a")])
        );
        remove_profile(&path, &config.id)?;
        let before = std::fs::read(&path)?;
        assert!(
            update_profile_metadata(&path, &config.id, Some(ConnectionColor::Red), None).is_err()
        );
        assert_eq!(std::fs::read(&path)?, before);
        Ok(())
    }
}

use crate::{Result, cloud::Home, control, session};
use serde::Serialize;
use std::fs::{self, File};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

#[derive(Serialize)]
pub struct Summary {
    pub id: String,
    pub name: String,
    pub device_count: usize,
    pub current: bool,
}

pub fn list() -> Result<Vec<Summary>> {
    let selected = current()?;
    Ok(control::cloud()?
        .homes()?
        .into_iter()
        .map(|home| {
            let active = selected
                .as_ref()
                .is_some_and(|current| current.id == home.id);
            summary(home, active)
        })
        .collect())
}

pub fn current() -> Result<Option<Home>> {
    load(&path()?)
}

pub fn current_summary() -> Result<Option<Summary>> {
    Ok(current()?.map(|home| summary(home, true)))
}

pub fn select(id: &str) -> Result<Summary> {
    let home = control::cloud()?.home(id)?;
    session::save_private_json(&path()?, &home)?;
    Ok(summary(home, true))
}

pub fn clear() -> Result<()> {
    match fs::remove_file(path()?) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn path() -> Result<PathBuf> {
    Ok(session::directory()?.join("home.json"))
}

fn load(path: &Path) -> Result<Option<Home>> {
    match File::open(path) {
        Ok(file) => serde_json::from_reader(file)
            .map(Some)
            .map_err(|_| "当前家庭配置无效，请运行 huajia home clear 或重新选择家庭".into()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn summary(home: Home, current: bool) -> Summary {
    Summary {
        id: home.id,
        name: home.name,
        device_count: home.device_ids.len(),
        current,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_back_the_saved_home_and_offline_membership() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("home.json");
        assert!(load(&path).unwrap().is_none());
        let home = Home {
            id: "1".into(),
            name: "家".into(),
            device_ids: ["123".into()].into(),
        };
        session::save_private_json(&path, &home).unwrap();
        let cached = load(&path).unwrap().unwrap();
        assert_eq!(cached.id, "1");
        assert!(cached.device_ids.contains("123"));
        assert!(summary(cached, true).current);
        fs::write(&path, "{}").unwrap();
        assert!(load(&path).is_err());
    }
}

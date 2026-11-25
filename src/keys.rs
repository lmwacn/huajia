use crate::{Result, session};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::path::{Path, PathBuf};
use subtle::ConstantTimeEq;

#[derive(Serialize)]
pub struct CreatedKey<'a> {
    pub name: &'a str,
    pub key: String,
}

#[derive(Deserialize, Serialize)]
struct KeyRecord {
    hash: [u8; 32],
}

pub struct KeyStore {
    directory: PathBuf,
}

impl KeyStore {
    pub fn new() -> Result<Self> {
        Ok(Self {
            directory: session::directory()?.join("keys"),
        })
    }

    pub fn create<'a>(&self, name: &'a str, rotate: bool) -> Result<CreatedKey<'a>> {
        let path = self
            .path(name)
            .ok_or("Key 名称须为 1 至 48 位字母、数字、- 或 _")?;
        if rotate {
            File::open(&path).map_err(|err| format!("无法读取待轮换的 Key {name:?}: {err}"))?;
        }
        let mut raw = [0u8; 32];
        getrandom::fill(&mut raw).map_err(|err| format!("无法生成 API Key: {err}"))?;
        let key = format!("omh_{name}.{}", URL_SAFE_NO_PAD.encode(raw));
        let record = KeyRecord {
            hash: Sha256::digest(key.as_bytes()).into(),
        };
        if rotate {
            session::save_private_json(&path, &record)?;
        } else {
            session::save_new_private_json(&path, &record).map_err(|err| {
                if path.is_file() {
                    format!("Key {name:?} 已存在，请用 huajia key rotate {name}")
                } else {
                    format!("无法保存 Key {name:?}: {err}")
                }
            })?;
        }
        Ok(CreatedKey { name, key })
    }

    pub fn authorized(&self, header: &str) -> bool {
        let Some((scheme, token)) = header.split_once(' ') else {
            return false;
        };
        if !scheme.eq_ignore_ascii_case("bearer") {
            return false;
        }
        let Some((name, rand)) = token
            .strip_prefix("omh_")
            .and_then(|token| token.split_once('.'))
        else {
            return false;
        };
        if rand.len() != 43
            || !rand
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            return false;
        }
        let Some(path) = self.path(name) else {
            return false;
        };
        let Ok(file) = File::open(path) else {
            return false;
        };
        let Ok(record) = serde_json::from_reader::<_, KeyRecord>(file) else {
            return false;
        };
        let actual: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        actual.ct_eq(&record.hash).into()
    }

    fn path(&self, name: &str) -> Option<PathBuf> {
        if name.is_empty()
            || name.len() > 48
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            return None;
        }
        Some(self.directory.join(Path::new(&format!("{name}.json"))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_is_no_clobber_and_rotation_revokes_the_previous_key() {
        let directory = tempfile::tempdir().unwrap();
        let store = KeyStore {
            directory: directory.path().join("keys"),
        };
        assert!(store.create("client", true).is_err());
        let old = store.create("client", false).unwrap().key;
        assert!(store.create("client", false).is_err());
        assert!(store.authorized(&format!("Bearer {old}")));
        assert!(!store.authorized(&old));
        assert!(!store.authorized("Bearer omh_../keys.invalid"));
        let new = store.create("client", true).unwrap().key;
        assert!(store.authorized(&format!("bearer {new}")));
        assert!(!store.authorized(&format!("Bearer {old}")));
        let saved = std::fs::read_to_string(store.path("client").unwrap()).unwrap();
        assert!(!saved.contains(&new));
    }
}

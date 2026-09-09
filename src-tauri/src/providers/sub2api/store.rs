use super::super::onenewapi::ids::new_id_avoiding;
use super::super::onenewapi::url::NormalizedUrl;
use super::{CreateSiteResult, KeyDto, SiteDto};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoreFile {
    pub version: u32,
    pub sites: Vec<SiteRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SiteRecord {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub next_key_ordinal: u32,
    pub keys: Vec<KeyRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct KeyRecord {
    pub id: String,
    pub label: String,
    pub api_key: String,
}

impl SiteRecord {
    pub fn to_dto(&self) -> SiteDto {
        SiteDto {
            id: self.id.clone(),
            name: self.name.clone(),
            base_url: self.base_url.clone(),
            keys: self.keys.iter().map(KeyRecord::to_dto).collect(),
        }
    }
}

impl KeyRecord {
    fn to_dto(&self) -> KeyDto {
        KeyDto {
            id: self.id.clone(),
            label: self.label.clone(),
            has_api_key: !self.api_key.is_empty(),
        }
    }
}

pub fn load(path: &Path) -> Result<StoreFile, String> {
    if !path.exists() {
        return Ok(StoreFile {
            version: 1,
            sites: Vec::new(),
        });
    }
    if path.is_file() {
        restrict_owner_only(path)?;
    }
    let raw = super::super::read_small_text(path, 1_048_576, "sub2api.json")?;
    let raw = raw.trim_start_matches('\u{feff}');
    let doc: StoreFile =
        serde_json::from_str(raw).map_err(|_| "sub2api.json is unreadable".to_string())?;
    if doc.version != 1 {
        return Err(format!(
            "sub2api.json has unsupported version {}",
            doc.version
        ));
    }
    Ok(doc)
}

pub fn save(path: &Path, doc: &StoreFile) -> Result<(), String> {
    if path.exists() {
        load(path)?;
    }
    let raw =
        serde_json::to_string_pretty(doc).map_err(|e| format!("serialize sub2api.json: {e}"))?;
    atomic_write(path, &raw)
}

use super::super::onenewapi::store::{atomic_write, restrict_owner_only};

fn occupied_ids(doc: &StoreFile) -> HashSet<String> {
    let mut ids = HashSet::new();
    for site in &doc.sites {
        ids.insert(site.id.clone());
        for key in &site.keys {
            ids.insert(key.id.clone());
        }
    }
    ids
}

fn display_name(name: &str, hostname: &str) -> String {
    let name = name.trim();
    if name.is_empty() {
        hostname.to_string()
    } else {
        name.to_string()
    }
}

pub fn list_sites(path: &Path) -> Result<Vec<SiteDto>, String> {
    Ok(load(path)?.sites.iter().map(SiteRecord::to_dto).collect())
}

pub fn insert_site(
    path: &Path,
    name: &str,
    normalized: &NormalizedUrl,
) -> Result<CreateSiteResult, String> {
    let mut doc = load(path)?;
    if let Some(existing) = doc.sites.iter().find(|s| s.base_url == normalized.origin) {
        return Ok(CreateSiteResult::Duplicate {
            site_id: existing.id.clone(),
        });
    }
    let id = new_id_avoiding(&occupied_ids(&doc))?;
    let site = SiteRecord {
        id,
        name: display_name(name, &normalized.hostname),
        base_url: normalized.origin.clone(),
        next_key_ordinal: 1,
        keys: Vec::new(),
    };
    let dto = site.to_dto();
    doc.sites.push(site);
    save(path, &doc)?;
    Ok(CreateSiteResult::Created { site: dto })
}

pub fn update_site(
    path: &Path,
    id: &str,
    name: Option<String>,
    new_url: Option<NormalizedUrl>,
) -> Result<SiteDto, String> {
    let mut doc = load(path)?;
    if let Some(ref n) = new_url {
        if doc
            .sites
            .iter()
            .any(|s| s.id != id && s.base_url == n.origin)
        {
            return Err("a site with this URL already exists".into());
        }
    }
    let site = doc
        .sites
        .iter_mut()
        .find(|s| s.id == id)
        .ok_or_else(|| "site not found".to_string())?;
    if let Some(n) = new_url {
        site.base_url = n.origin;
        if let Some(ref name) = name {
            site.name = display_name(name, &n.hostname);
        }
    } else if let Some(ref name) = name {
        let hostname = reqwest::Url::parse(&site.base_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| site.base_url.clone());
        site.name = display_name(name, &hostname);
    }
    let dto = site.to_dto();
    save(path, &doc)?;
    Ok(dto)
}

pub fn delete_site(path: &Path, id: &str) -> Result<(), String> {
    let mut doc = load(path)?;
    let before = doc.sites.len();
    doc.sites.retain(|s| s.id != id);
    if doc.sites.len() == before {
        return Err("site not found".into());
    }
    save(path, &doc)
}

fn total_keys(doc: &StoreFile) -> usize {
    doc.sites.iter().map(|s| s.keys.len()).sum()
}

fn assign_label(
    site: &mut SiteRecord,
    label: &str,
    exclude_id: Option<&str>,
) -> Result<String, String> {
    let taken = |site: &SiteRecord, candidate: &str| {
        site.keys
            .iter()
            .any(|k| exclude_id != Some(k.id.as_str()) && k.label == candidate)
    };
    let trimmed = label.trim();
    if !trimmed.is_empty() {
        if taken(site, trimmed) {
            return Err("a key with this label already exists on this site".into());
        }
        return Ok(trimmed.to_string());
    }
    loop {
        let n = site.next_key_ordinal;
        let candidate = format!("Key {n}");
        site.next_key_ordinal = n
            .checked_add(1)
            .ok_or_else(|| "could not allocate a key label".to_string())?;
        if !taken(site, &candidate) {
            return Ok(candidate);
        }
    }
}

pub fn create_key(
    path: &Path,
    site_id: &str,
    label: &str,
    api_key: &str,
) -> Result<super::CreatedKey, String> {
    let secret = api_key.trim();
    if secret.is_empty() {
        return Err("API key is required".into());
    }
    let mut doc = load(path)?;
    let first_key = total_keys(&doc) == 0;
    let occupied = occupied_ids(&doc);
    let site = doc
        .sites
        .iter_mut()
        .find(|s| s.id == site_id)
        .ok_or_else(|| "site not found".to_string())?;
    if site.keys.iter().any(|k| k.api_key == secret) {
        return Err("this API key is already saved on this site".into());
    }
    let id = new_id_avoiding(&occupied)?;
    let label = assign_label(site, label, None)?;
    site.keys.push(KeyRecord {
        id: id.clone(),
        label,
        api_key: secret.to_string(),
    });
    let dto = site.to_dto();
    save(path, &doc)?;
    Ok(super::CreatedKey {
        site: dto,
        key_id: id,
        first_key,
    })
}

pub fn update_key(
    path: &Path,
    site_id: &str,
    key_id: &str,
    label: Option<String>,
    api_key: Option<String>,
) -> Result<super::SiteDto, String> {
    let mut doc = load(path)?;
    let site = doc
        .sites
        .iter_mut()
        .find(|s| s.id == site_id)
        .ok_or_else(|| "site not found".to_string())?;
    let idx = site
        .keys
        .iter()
        .position(|k| k.id == key_id)
        .ok_or_else(|| "key not found".to_string())?;
    if let Some(ref raw) = api_key {
        let secret = raw.trim();
        if !secret.is_empty() {
            if site
                .keys
                .iter()
                .any(|k| k.id != key_id && k.api_key == secret)
            {
                return Err("this API key is already saved on this site".into());
            }
            site.keys[idx].api_key = secret.to_string();
        }
    }
    if let Some(ref raw) = label {
        let assigned = assign_label(site, raw, Some(key_id))?;
        site.keys[idx].label = assigned;
    }
    let dto = site.to_dto();
    save(path, &doc)?;
    Ok(dto)
}

pub fn delete_key(path: &Path, site_id: &str, key_id: &str) -> Result<super::SiteDto, String> {
    let mut doc = load(path)?;
    let site = doc
        .sites
        .iter_mut()
        .find(|s| s.id == site_id)
        .ok_or_else(|| "site not found".to_string())?;
    let before = site.keys.len();
    site.keys.retain(|k| k.id != key_id);
    if site.keys.len() == before {
        return Err("key not found".into());
    }
    let dto = site.to_dto();
    save(path, &doc)?;
    Ok(dto)
}

mod snapshot;
mod store;
use super::onenewapi::url;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SiteDto {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub keys: Vec<KeyDto>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct KeyDto {
    pub id: String,
    pub label: String,
    pub has_api_key: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "status")]
pub enum CreateSiteResult {
    #[serde(rename = "created")]
    Created { site: SiteDto },
    #[serde(rename = "duplicate")]
    Duplicate { site_id: String },
}

fn store_path() -> PathBuf {
    super::config_dir().join("sub2api.json")
}

fn store_mutation_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn lock_store_mutation() -> Result<MutexGuard<'static, ()>, String> {
    store_mutation_lock()
        .lock()
        .map_err(|_| "sub2api store mutation lock is poisoned".to_string())
}

/// Serializes a primary store write with its derived-state cleanup. Cleanup
/// runs only after the atomic store write succeeds; if cleanup fails, the
/// complete credential document is restored before the error is returned.
/// The cleanup closure must not re-enter this module's store interface.
fn coordinate_store_mutation_at<T, Mutate, Cleanup>(
    path: &Path,
    mutate: Mutate,
    cleanup: Cleanup,
) -> Result<T, String>
where
    Mutate: FnOnce(&Path) -> Result<T, String>,
    Cleanup: FnOnce(&T) -> Result<(), String>,
{
    let _lock = lock_store_mutation()?;
    let checkpoint = store::load(path)?;
    let result = mutate(path)?;
    if let Err(cleanup_error) = cleanup(&result) {
        return match store::save(path, &checkpoint) {
            Ok(()) => Err(cleanup_error),
            Err(rollback_error) => Err(format!(
                "{cleanup_error}; rollback sub2api.json failed: {rollback_error}"
            )),
        };
    }
    Ok(result)
}

pub fn list_sites() -> Result<Vec<SiteDto>, String> {
    let _lock = lock_store_mutation()?;
    store::list_sites(&store_path())
}

pub async fn create_site(name: String, base_url: String) -> Result<CreateSiteResult, String> {
    create_site_at(&store_path(), name, base_url).await
}

pub async fn create_site_at(
    path: &Path,
    name: String,
    base_url: String,
) -> Result<CreateSiteResult, String> {
    let normalized = url::normalize_base_url(&base_url)?;
    let existing = store::load(path)?;
    if let Some(site) = existing
        .sites
        .iter()
        .find(|s| s.base_url == normalized.origin)
    {
        return Ok(CreateSiteResult::Duplicate {
            site_id: site.id.clone(),
        });
    }
    let _lock = lock_store_mutation()?;
    store::insert_site(path, &name, &normalized)
}

pub(crate) fn normalize_site_url(base_url: &str) -> Result<String, String> {
    Ok(url::normalize_base_url(base_url)?.origin)
}

/// Commits a locally validated site edit,
/// then applies its derived-state cleanup before exposing success.
pub(crate) fn update_site_consistently<Cleanup>(
    id: String,
    name: Option<String>,
    base_url: Option<String>,
    cleanup: Cleanup,
) -> Result<SiteDto, String>
where
    Cleanup: FnOnce(&SiteDto) -> Result<(), String>,
{
    let normalized = base_url
        .as_deref()
        .map(url::normalize_base_url)
        .transpose()?;
    coordinate_store_mutation_at(
        &store_path(),
        |path| store::update_site(path, &id, name, normalized),
        cleanup,
    )
}

#[cfg(test)]
async fn update_site_at(
    path: &Path,
    id: &str,
    name: Option<String>,
    base_url: Option<String>,
) -> Result<SiteDto, String> {
    let normalized = base_url
        .as_deref()
        .map(url::normalize_base_url)
        .transpose()?;
    let _lock = lock_store_mutation()?;
    store::update_site(path, id, name, normalized)
}

pub(crate) fn delete_site_consistently<Cleanup>(id: String, cleanup: Cleanup) -> Result<(), String>
where
    Cleanup: FnOnce() -> Result<(), String>,
{
    coordinate_store_mutation_at(
        &store_path(),
        |path| store::delete_site(path, &id),
        |_| cleanup(),
    )
}

#[derive(Debug, Serialize)]
pub struct CreatedKey {
    pub site: SiteDto,
    pub key_id: String,
    pub first_key: bool,
}

pub fn create_key(site_id: String, label: String, api_key: String) -> Result<CreatedKey, String> {
    let _lock = lock_store_mutation()?;
    store::create_key(&store_path(), &site_id, &label, &api_key)
}

pub(crate) fn update_key_consistently<Cleanup>(
    site_id: String,
    key_id: String,
    label: Option<String>,
    api_key: Option<String>,
    cleanup: Cleanup,
) -> Result<SiteDto, String>
where
    Cleanup: FnOnce(&SiteDto) -> Result<(), String>,
{
    coordinate_store_mutation_at(
        &store_path(),
        |path| store::update_key(path, &site_id, &key_id, label, api_key),
        cleanup,
    )
}

pub(crate) fn delete_key_consistently<Cleanup>(
    site_id: String,
    key_id: String,
    cleanup: Cleanup,
) -> Result<SiteDto, String>
where
    Cleanup: FnOnce() -> Result<(), String>,
{
    coordinate_store_mutation_at(
        &store_path(),
        |path| store::delete_key(path, &site_id, &key_id),
        |_| cleanup(),
    )
}

pub use snapshot::{refresh_clients, snapshot_key_with_client, KeyCard};

pub fn key_cards() -> Result<Vec<KeyCard>, String> {
    let _lock = lock_store_mutation()?;
    snapshot::key_cards_at(&store_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempStore {
        directory: PathBuf,
        path: PathBuf,
    }
    impl TempStore {
        fn new() -> Self {
            let directory = std::env::temp_dir().join(format!(
                "pane-sub2api-{}",
                super::super::onenewapi::ids::new_id().unwrap()
            ));
            std::fs::create_dir_all(&directory).unwrap();
            Self {
                path: directory.join("sub2api.json"),
                directory,
            }
        }
    }
    impl Drop for TempStore {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    async fn site(path: &Path, name: &str, url: &str) -> SiteDto {
        match create_site_at(path, name.into(), url.into()).await.unwrap() {
            CreateSiteResult::Created { site } => site,
            _ => panic!("duplicate fixture"),
        }
    }

    #[tokio::test]
    async fn local_save_needs_no_probe_and_auth_failure_keeps_redacted_key() {
        let temp = TempStore::new();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", server.server_addr());
        let site = site(&temp.path, "", &format!("{origin}/v1/")).await;
        assert!(site.keys.is_empty());
        assert!(snapshot::key_cards_at(&temp.path).unwrap().is_empty());
        let key = store::create_key(&temp.path, &site.id, "", "sk-live-private-value-abc").unwrap();
        assert!(key.first_key);
        assert_eq!(key.site.keys[0].label, "Key 1");
        assert!(server
            .recv_timeout(std::time::Duration::from_millis(20))
            .unwrap()
            .is_none());
        let handle = std::thread::spawn(move || {
            let request = server
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
                .unwrap();
            assert_eq!(request.url(), "/v1/usage");
            request
                .respond(
                    tiny_http::Response::from_string("secret remote error").with_status_code(401),
                )
                .unwrap();
        });
        let card = snapshot::key_cards_at(&temp.path).unwrap().remove(0);
        assert_eq!(card.id, format!("sub2api@{}", key.key_id));
        let observed = snapshot_key_with_client(super::super::http_no_redirect(), card).await;
        handle.join().unwrap();
        assert_eq!(observed.status, "error");
        let listed = store::list_sites(&temp.path).unwrap();
        assert_eq!(listed[0].keys[0].id, key.key_id);
        let public = serde_json::to_string(&listed).unwrap();
        assert!(!public.contains("private-value"));
        assert!(!public.contains("\"api_key\":"));
        assert!(listed[0].keys[0].has_api_key);
    }

    #[tokio::test]
    async fn local_crud_keeps_identity_ordinals_and_independent_sites() {
        let temp = TempStore::new();
        let first = site(&temp.path, "", "https://one.example/v1").await;
        assert_eq!(first.name, "one.example");
        assert!(matches!(
            create_site_at(
                &temp.path,
                "Duplicate".into(),
                "https://one.example/".into()
            )
            .await
            .unwrap(),
            CreateSiteResult::Duplicate { .. }
        ));
        let second = site(&temp.path, "Two", "https://two.example").await;
        let key = store::create_key(&temp.path, &first.id, "", "secret-a").unwrap();
        assert!(store::create_key(&temp.path, &first.id, "Different", "secret-a").is_err());
        assert!(store::create_key(&temp.path, &first.id, "Key 1", "secret-b").is_err());
        assert!(store::create_key(&temp.path, &first.id, "", "  ").is_err());
        assert!(
            !store::create_key(&temp.path, &second.id, "", "secret-a")
                .unwrap()
                .first_key
        );
        let renamed = store::update_key(
            &temp.path,
            &first.id,
            &key.key_id,
            Some("Renamed".into()),
            Some(" ".into()),
        )
        .unwrap();
        assert_eq!(renamed.keys[0].id, key.key_id);
        assert_eq!(
            snapshot::key_cards_at(&temp.path)
                .unwrap()
                .iter()
                .find(|c| c.id == format!("sub2api@{}", key.key_id))
                .unwrap()
                .api_key,
            "secret-a"
        );
        let rotated = store::update_key(
            &temp.path,
            &first.id,
            &key.key_id,
            None,
            Some("secret-c".into()),
        )
        .unwrap();
        assert_eq!(rotated.keys[0].id, key.key_id);
        store::delete_key(&temp.path, &first.id, &key.key_id).unwrap();
        let next = store::create_key(&temp.path, &first.id, "", "secret-d").unwrap();
        assert_eq!(next.site.keys[0].label, "Key 2");
        update_site_at(
            &temp.path,
            &first.id,
            Some("New".into()),
            Some("https://new.example/v1".into()),
        )
        .await
        .unwrap();
        store::delete_site(&temp.path, &first.id).unwrap();
        assert_eq!(store::list_sites(&temp.path).unwrap()[0].id, second.id);
    }

    #[tokio::test]
    async fn corrupt_store_and_cleanup_failures_never_claim_success() {
        let temp = TempStore::new();
        std::fs::write(&temp.path, "corrupt-private-data").unwrap();
        assert!(
            create_site_at(&temp.path, "".into(), "https://example.com".into())
                .await
                .is_err()
        );
        assert!(store::create_key(&temp.path, "site", "", "key").is_err());
        assert_eq!(
            std::fs::read_to_string(&temp.path).unwrap(),
            "corrupt-private-data"
        );
        std::fs::remove_file(&temp.path).unwrap();
        let site = site(&temp.path, "Site", "https://example.com").await;
        let before = std::fs::read_to_string(&temp.path).unwrap();
        let result = coordinate_store_mutation_at(
            &temp.path,
            |p| store::delete_site(p, &site.id),
            |_| Err("cleanup failed".into()),
        );
        assert_eq!(result.unwrap_err(), "cleanup failed");
        assert_eq!(std::fs::read_to_string(&temp.path).unwrap(), before);
        let cleaned = std::cell::Cell::new(false);
        assert!(coordinate_store_mutation_at(
            &temp.path,
            |p| store::delete_site(p, "missing"),
            |_| {
                cleaned.set(true);
                Ok(())
            }
        )
        .is_err());
        assert!(!cleaned.get());
    }
}

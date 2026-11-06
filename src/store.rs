use std::{
    fs::OpenOptions,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail, ensure};
use redb::{
    Database, Durability, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition,
};
use uuid::Uuid;

use crate::conditions::{Conditions, Decision};

const DOCUMENTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("documents_v1");
const METADATA: TableDefinition<&[u8], &[u8]> = TableDefinition::new("metadata_v1");
const SYSTEM: TableDefinition<&str, &[u8]> = TableDefinition::new("system_v1");
const STATE_KEY: &str = "identity_and_revision";

pub struct Store {
    database: Database,
    identity: Uuid,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Metadata {
    pub revision: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub size: u64,
}

pub(crate) struct Document {
    pub metadata: Metadata,
    pub bytes: Option<Vec<u8>>,
}

pub(crate) enum Mutation {
    Written { created: bool, metadata: Metadata },
    Deleted,
    PreconditionFailed,
}

impl Store {
    pub fn open(path: impl AsRef<Path>, cache_bytes: usize) -> Result<Self> {
        let path = path.as_ref();
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        let database = Database::builder()
            .set_cache_size(cache_bytes)
            .create_file(file)
            .with_context(|| format!("opening redb at {}", path.display()))?;
        let store = Self::initialize(database)?;
        #[cfg(unix)]
        {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            std::fs::File::open(parent)?
                .sync_all()
                .with_context(|| format!("syncing database directory {}", parent.display()))?;
        }
        Ok(store)
    }

    fn initialize(database: Database) -> Result<Self> {
        let mut transaction = database.begin_write()?;
        transaction.set_durability(Durability::Immediate)?;
        let identity;
        {
            let documents = transaction.open_table(DOCUMENTS)?;
            let metadata = transaction.open_table(METADATA)?;
            let mut system = transaction.open_table(SYSTEM)?;
            let existing = system.get(STATE_KEY)?.map(|value| value.value().to_vec());
            identity = if let Some(state) = existing {
                decode_state(&state)?.0
            } else {
                ensure!(
                    documents.is_empty()? && metadata.is_empty()?,
                    "database is missing its revision state"
                );
                let id = Uuid::new_v4();
                let state = encode_state(id, 0);
                system.insert(STATE_KEY, state.as_slice())?;
                id
            };
        }
        transaction
            .commit()
            .context("committing database initialization")?;
        Ok(Self { database, identity })
    }

    pub(crate) fn etag(&self, metadata: Metadata) -> String {
        format!("\"{}-{}\"", self.identity.simple(), metadata.revision)
    }

    pub(crate) fn read(
        &self,
        id: Uuid,
        include_bytes: bool,
        conditions: &Conditions,
    ) -> Result<Option<(Document, Decision)>> {
        let transaction = self.database.begin_read()?;
        let metadata = transaction.open_table(METADATA)?;
        let documents = transaction.open_table(DOCUMENTS)?;
        let meta = metadata.get(id.as_bytes().as_slice())?;
        let bytes = documents.get(id.as_bytes().as_slice())?;
        match (meta, bytes) {
            (None, None) => Ok(None),
            (Some(meta), Some(bytes)) => {
                let metadata = Metadata::decode(meta.value())?;
                ensure!(
                    metadata.size == bytes.value().len() as u64,
                    "document length differs from metadata"
                );
                let decision = conditions.evaluate(Some(&self.etag(metadata)), true);
                Ok(Some((
                    Document {
                        metadata,
                        bytes: (include_bytes && decision == Decision::Proceed)
                            .then(|| bytes.value().to_vec()),
                    },
                    decision,
                )))
            }
            _ => bail!("document and metadata tables are inconsistent"),
        }
    }

    pub(crate) fn put(&self, id: Uuid, bytes: &[u8], conditions: &Conditions) -> Result<Mutation> {
        let mut transaction = self.database.begin_write()?;
        transaction.set_durability(Durability::Immediate)?;
        let result;
        {
            let mut documents = transaction.open_table(DOCUMENTS)?;
            let mut metadata = transaction.open_table(METADATA)?;
            let mut system = transaction.open_table(SYSTEM)?;
            let previous = metadata
                .get(id.as_bytes().as_slice())?
                .map(|value| Metadata::decode(value.value()))
                .transpose()?;
            let document_size = documents
                .get(id.as_bytes().as_slice())?
                .map(|value| value.value().len() as u64);
            ensure!(
                previous.map(|value| value.size) == document_size,
                "document and metadata tables are inconsistent"
            );
            let etag = previous.map(|value| self.etag(value));
            if conditions.evaluate(etag.as_deref(), false) == Decision::Failed {
                return Ok(Mutation::PreconditionFailed);
            }
            let state = system
                .get(STATE_KEY)?
                .context("missing database revision state")?;
            let (identity, revision) = decode_state(state.value())?;
            ensure!(identity == self.identity, "database identity changed");
            let revision = revision
                .checked_add(1)
                .context("database revision counter exhausted")?;
            drop(state);
            let now = timestamp_ms()?;
            let updated = previous.map_or(now, |value| now.max(value.updated_at_ms));
            let next = Metadata {
                revision,
                created_at_ms: previous.map_or(now, |value| value.created_at_ms),
                updated_at_ms: updated,
                size: bytes.len() as u64,
            };
            let encoded = next.encode();
            documents.insert(id.as_bytes().as_slice(), bytes)?;
            metadata.insert(id.as_bytes().as_slice(), encoded.as_slice())?;
            let state = encode_state(identity, revision);
            system.insert(STATE_KEY, state.as_slice())?;
            result = Mutation::Written {
                created: previous.is_none(),
                metadata: next,
            };
        }
        transaction
            .commit()
            .context("durably committing document replacement")?;
        Ok(result)
    }

    pub(crate) fn delete(&self, id: Uuid, conditions: &Conditions) -> Result<Mutation> {
        let mut transaction = self.database.begin_write()?;
        transaction.set_durability(Durability::Immediate)?;
        {
            let mut documents = transaction.open_table(DOCUMENTS)?;
            let mut metadata = transaction.open_table(METADATA)?;
            let previous = metadata
                .get(id.as_bytes().as_slice())?
                .map(|value| Metadata::decode(value.value()))
                .transpose()?;
            let document_size = documents
                .get(id.as_bytes().as_slice())?
                .map(|value| value.value().len() as u64);
            ensure!(
                previous.map(|value| value.size) == document_size,
                "document and metadata tables are inconsistent"
            );
            let etag = previous.map(|value| self.etag(value));
            if conditions.evaluate(etag.as_deref(), false) == Decision::Failed {
                return Ok(Mutation::PreconditionFailed);
            }
            if previous.is_none() {
                return Ok(Mutation::Deleted);
            }
            documents.remove(id.as_bytes().as_slice())?;
            metadata.remove(id.as_bytes().as_slice())?;
        }
        transaction
            .commit()
            .context("durably committing document deletion")?;
        Ok(Mutation::Deleted)
    }
}

impl Metadata {
    fn encode(self) -> [u8; 32] {
        let mut bytes = [0; 32];
        for (chunk, value) in bytes.as_chunks_mut::<8>().0.iter_mut().zip([
            self.revision,
            self.created_at_ms,
            self.updated_at_ms,
            self.size,
        ]) {
            chunk.copy_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(bytes.len() == 32, "invalid metadata record");
        let value =
            |start| -> Result<u64> { Ok(u64::from_le_bytes(bytes[start..start + 8].try_into()?)) };
        let metadata = Self {
            revision: value(0)?,
            created_at_ms: value(8)?,
            updated_at_ms: value(16)?,
            size: value(24)?,
        };
        ensure!(
            metadata.revision != 0 && metadata.updated_at_ms >= metadata.created_at_ms,
            "invalid metadata values"
        );
        Ok(metadata)
    }
}

fn encode_state(identity: Uuid, revision: u64) -> [u8; 24] {
    let mut state = [0; 24];
    state[..16].copy_from_slice(identity.as_bytes());
    state[16..].copy_from_slice(&revision.to_le_bytes());
    state
}

fn decode_state(state: &[u8]) -> Result<(Uuid, u64)> {
    ensure!(state.len() == 24, "invalid database revision state");
    Ok((
        Uuid::from_slice(&state[..16])?,
        u64::from_le_bytes(state[16..].try_into()?),
    ))
}

fn timestamp_ms() -> Result<u64> {
    u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())
        .context("system timestamp is out of range")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use redb::{StorageBackend, backends::FileBackend};
    use tower::ServiceExt;

    #[test]
    fn revisions_survive_deletion_recreation_and_restarts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("store.redb");
        let id = Uuid::new_v4();
        let store = Store::open(&path, 1024 * 1024).unwrap();
        store.put(id, b"1", &Conditions::default()).unwrap();
        let old_tag = store.etag(
            store
                .read(id, false, &Conditions::default())
                .unwrap()
                .unwrap()
                .0
                .metadata,
        );
        store.delete(id, &Conditions::default()).unwrap();
        drop(store);
        let store = Store::open(&path, 1024 * 1024).unwrap();
        assert!(
            store
                .read(id, true, &Conditions::default())
                .unwrap()
                .is_none()
        );
        store.put(id, b"2", &Conditions::default()).unwrap();
        let document = store
            .read(id, true, &Conditions::default())
            .unwrap()
            .unwrap()
            .0;
        assert_eq!(document.bytes.unwrap(), b"2");
        assert_ne!(store.etag(document.metadata), old_tag);
    }

    #[test]
    fn corrupt_files_and_second_writers_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("store.redb");
        let store = Store::open(&path, 1024 * 1024).unwrap();
        assert!(Store::open(&path, 1024 * 1024).is_err());
        drop(store);
        let invalid = directory.path().join("invalid.redb");
        std::fs::write(&invalid, b"not a redb database").unwrap();
        assert!(Store::open(&invalid, 1024 * 1024).is_err());
    }

    #[test]
    fn exhausted_revision_counter_fails_without_wrapping_or_mutating() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("revision.redb"), 1024 * 1024).unwrap();
        let id = Uuid::new_v4();
        store.put(id, b"1", &Conditions::default()).unwrap();
        let old = store
            .read(id, true, &Conditions::default())
            .unwrap()
            .unwrap()
            .0;
        let mut transaction = store.database.begin_write().unwrap();
        transaction.set_durability(Durability::Immediate).unwrap();
        {
            let mut system = transaction.open_table(SYSTEM).unwrap();
            system
                .insert(STATE_KEY, encode_state(store.identity, u64::MAX).as_slice())
                .unwrap();
        }
        transaction.commit().unwrap();
        assert!(store.put(id, b"2", &Conditions::default()).is_err());
        let after = store
            .read(id, true, &Conditions::default())
            .unwrap()
            .unwrap()
            .0;
        assert_eq!(store.etag(old.metadata), store.etag(after.metadata));
        assert_eq!(old.bytes, after.bytes);
    }

    #[test]
    fn different_database_incarnations_never_reuse_etags() {
        let directory = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        let mut tags = Vec::new();
        for filename in ["first.redb", "second.redb"] {
            let store = Store::open(directory.path().join(filename), 1024 * 1024).unwrap();
            store.put(id, b"1", &Conditions::default()).unwrap();
            tags.push(
                store.etag(
                    store
                        .read(id, false, &Conditions::default())
                        .unwrap()
                        .unwrap()
                        .0
                        .metadata,
                ),
            );
        }
        assert_ne!(tags[0], tags[1]);
    }

    #[cfg(unix)]
    #[test]
    fn newly_created_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("private.redb");
        let _store = Store::open(&path, 1024 * 1024).unwrap();
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[derive(Debug)]
    struct FaultBackend {
        inner: FileBackend,
        fail_sync: Arc<AtomicBool>,
        syncs: Arc<AtomicUsize>,
    }

    impl StorageBackend for FaultBackend {
        fn len(&self) -> io::Result<u64> {
            self.inner.len()
        }

        fn read(&self, offset: u64, output: &mut [u8]) -> io::Result<()> {
            self.inner.read(offset, output)
        }

        fn set_len(&self, length: u64) -> io::Result<()> {
            self.inner.set_len(length)
        }

        fn sync_data(&self) -> io::Result<()> {
            self.syncs.fetch_add(1, Ordering::SeqCst);
            if self.fail_sync.load(Ordering::SeqCst) {
                Err(io::Error::other("injected durable commit failure"))
            } else {
                self.inner.sync_data()
            }
        }

        fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
            self.inner.write(offset, data)
        }

        fn close(&self) -> io::Result<()> {
            self.inner.close()
        }
    }

    #[tokio::test]
    async fn failed_durable_commits_never_acknowledge_post_put_or_delete() {
        for method in ["POST", "PUT", "DELETE"] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("fault.redb");
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap();
            let fail_sync = Arc::new(AtomicBool::new(false));
            let syncs = Arc::new(AtomicUsize::new(0));
            let backend = FaultBackend {
                inner: FileBackend::new(file).unwrap(),
                fail_sync: fail_sync.clone(),
                syncs: syncs.clone(),
            };
            let database = Database::builder()
                .set_cache_size(1024 * 1024)
                .create_with_backend(backend)
                .unwrap();
            let store = Arc::new(Store::initialize(database).unwrap());
            let id = Uuid::new_v4();
            store.put(id, b"1", &Conditions::default()).unwrap();
            let config = crate::Config {
                allow_unauthenticated: true,
                ..crate::Config::default()
            };
            let router = crate::app(config, store.clone()).unwrap();
            let before = syncs.load(Ordering::SeqCst);
            fail_sync.store(true, Ordering::SeqCst);
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(if method == "POST" {
                            "/v1/objects".to_owned()
                        } else {
                            format!("/v1/objects/{id}")
                        })
                        .header("content-type", "application/json")
                        .body(Body::from("222"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
            let body = to_bytes(response.into_body(), 1024).await.unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&body).unwrap()["error"]["code"],
                "storage_error"
            );
            assert!(syncs.load(Ordering::SeqCst) > before);
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/v1/objects/{id}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            if response.status() == StatusCode::OK {
                let body = to_bytes(response.into_body(), 1024).await.unwrap();
                assert!(body == b"1".as_slice() || (method == "PUT" && body == b"222".as_slice()));
            } else {
                assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
            }
            drop(router);
            fail_sync.store(false, Ordering::SeqCst);
            drop(store);
            let store = Store::open(&path, 1024 * 1024).unwrap();
            let document = store.read(id, true, &Conditions::default()).unwrap();
            if method == "PUT" {
                let bytes = document.unwrap().0.bytes.unwrap();
                assert!(bytes == b"1" || bytes == b"222");
            } else if method == "POST" {
                assert_eq!(document.unwrap().0.bytes.unwrap(), b"1");
            } else if let Some((document, _)) = document {
                assert_eq!(document.bytes.unwrap(), b"1");
            }
        }
    }
}

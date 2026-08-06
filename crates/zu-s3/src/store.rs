//! Manifest commit protocol: conditional-put CAS on `manifest/CURRENT` with epoch fencing.
//!
//! Every commit writes an immutable snapshot at `manifest/{epoch:020}.zum` and then swings `manifest/CURRENT` with a conditional put, so a torn commit is invisible until the swap.
//! `CURRENT` holds the full encoded manifest rather than a pointer, which keeps open at one round trip and leaves one object to CAS; the epoch-named snapshots provide the immutable history.
//! Fencing follows the SlateDB sequence in `docs/06-storage-s3.md` section 3: epochs advance by exactly one, only the installed writer may commit, and a superseded writer's conditional put fails permanently.
//! Snapshots are written with `PutMode::Create`; an orphan snapshot left by a crashed writer blocks that epoch until GC reclaims it, which lands with the grace-list GC.

use std::sync::Arc;

use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutPayload, UpdateVersion};
use zu_common::{Result, ZuError};

use crate::manifest::Manifest;
use crate::rt::block_on;

/// Object key of the CAS-updated current-manifest object.
pub const CURRENT_KEY: &str = "manifest/CURRENT";

/// Object key of the immutable snapshot for `epoch`.
fn snapshot_key(epoch: u64) -> String {
    format!("manifest/{epoch:020}.zum")
}

/// A manifest read from `manifest/CURRENT` plus the CAS token needed to replace it.
#[derive(Clone, Debug)]
pub struct Current {
    pub manifest: Manifest,
    version: UpdateVersion,
}

/// Reads and commits manifests on behalf of one writer identity.
pub struct ManifestStore {
    store: Arc<dyn ObjectStore>,
    writer_id: u128,
}

impl ManifestStore {
    pub fn new(store: Arc<dyn ObjectStore>, writer_id: u128) -> Self {
        Self { store, writer_id }
    }

    pub fn writer_id(&self) -> u128 {
        self.writer_id
    }

    /// Loads `manifest/CURRENT`, returning `None` if no manifest was ever committed.
    pub fn read_current(&self) -> Result<Option<Current>> {
        let result = match block_on(self.store.get(&Path::from(CURRENT_KEY))) {
            Ok(result) => result,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(err) => return Err(store_err(err)),
        };
        let version = UpdateVersion {
            e_tag: result.meta.e_tag.clone(),
            version: result.meta.version.clone(),
        };
        let bytes = block_on(result.bytes()).map_err(store_err)?;
        let manifest = Manifest::decode(&bytes)?;
        Ok(Some(Current { manifest, version }))
    }

    /// Commits `next` on top of `expected`, the `Current` this writer last observed.
    ///
    /// The first commit passes `None` and must carry epoch 0; every later
    /// commit must advance the epoch by exactly one. Losing the CAS race,
    /// or holding a manifest owned by another writer, returns
    /// `ZuError::Conflict`; recover by re-reading or by [`Self::take_over`].
    pub fn commit(&self, next: &Manifest, expected: Option<&Current>) -> Result<Current> {
        if next.writer_id != self.writer_id {
            return Err(ZuError::InvalidArgument(format!(
                "manifest writer_id {:#x} does not match this store's {:#x}",
                next.writer_id, self.writer_id
            )));
        }
        let Some(current) = expected else {
            if next.epoch != 0 {
                return Err(ZuError::InvalidArgument(format!(
                    "first manifest commit must use epoch 0, got {}",
                    next.epoch
                )));
            }
            return self.publish(next, None);
        };
        if current.manifest.writer_id != self.writer_id {
            return Err(ZuError::Conflict(format!(
                "fenced: writer {:#x} owns the manifest, take_over required",
                current.manifest.writer_id
            )));
        }
        let want = current
            .manifest
            .epoch
            .checked_add(1)
            .ok_or_else(|| ZuError::InvalidArgument("manifest epoch overflow".to_string()))?;
        if next.epoch != want {
            return Err(ZuError::InvalidArgument(format!(
                "manifest epoch must advance from {} to {want}, got {}",
                current.manifest.epoch, next.epoch
            )));
        }
        self.publish(next, Some(&current.version))
    }

    /// Takes over writership from whoever holds it, per the SlateDB sequence.
    ///
    /// Reads `CURRENT`, re-publishes the same segment set under the next
    /// epoch with this store's `writer_id`, and CAS-swaps `CURRENT`.
    /// Success installs this store as the sole writer; every conditional put
    /// by the previous writer fails from then on. A lost race returns
    /// `ZuError::Conflict` and the caller may retry.
    pub fn take_over(&self) -> Result<Current> {
        let current = self.read_current()?.ok_or_else(|| {
            ZuError::InvalidArgument("cannot take over: no CURRENT manifest exists".to_string())
        })?;
        let epoch = current
            .manifest
            .epoch
            .checked_add(1)
            .ok_or_else(|| ZuError::InvalidArgument("manifest epoch overflow".to_string()))?;
        let next = Manifest {
            epoch,
            writer_id: self.writer_id,
            segments: current.manifest.segments.clone(),
        };
        self.publish(&next, Some(&current.version))
    }

    /// Writes the immutable snapshot, then swings `CURRENT` conditionally.
    fn publish(&self, next: &Manifest, expected: Option<&UpdateVersion>) -> Result<Current> {
        let bytes = next.encode()?;
        let snapshot = Path::from(snapshot_key(next.epoch));
        block_on(self.store.put_opts(
            &snapshot,
            PutPayload::from(bytes.clone()),
            PutMode::Create.into(),
        ))
        .map_err(store_err)?;
        let mode = match expected {
            Some(version) => PutMode::Update(version.clone()),
            None => PutMode::Create,
        };
        let result = block_on(self.store.put_opts(
            &Path::from(CURRENT_KEY),
            PutPayload::from(bytes),
            mode.into(),
        ))
        .map_err(store_err)?;
        Ok(Current {
            manifest: next.clone(),
            version: result.into(),
        })
    }
}

/// Maps object-store failures: a failed precondition or an already-existing
/// object means another writer got there first, everything else is IO.
fn store_err(err: object_store::Error) -> ZuError {
    match err {
        object_store::Error::Precondition { .. } | object_store::Error::AlreadyExists { .. } => {
            ZuError::Conflict(err.to_string())
        }
        other => ZuError::Io(std::io::Error::other(other)),
    }
}

#[cfg(test)]
mod tests {
    use object_store::local::LocalFileSystem;
    use object_store::memory::InMemory;

    use super::*;

    /// Runs `test` against every backend that supports the conditional put
    /// modes the commit protocol needs.
    /// Only `InMemory` qualifies today: `LocalFileSystem` in object_store
    /// 0.14 rejects `PutMode::Update` with `NotImplemented` in `put_opts`,
    /// so it cannot host the CURRENT swap. The suite therefore runs on
    /// `InMemory` alone and `local_fs_lacks_conditional_update` pins the
    /// limitation so a future object_store upgrade flags it.
    fn each_backend(test: impl Fn(Arc<dyn ObjectStore>)) {
        test(Arc::new(InMemory::new()));
    }

    fn manifest(epoch: u64, writer_id: u128, segments: &[&str]) -> Manifest {
        Manifest {
            epoch,
            writer_id,
            segments: segments.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn commit_chain_of_three_epochs() {
        each_backend(|backend| {
            let store = ManifestStore::new(backend.clone(), 1);
            assert!(store.read_current().unwrap().is_none());
            let mut current = store
                .commit(&manifest(0, 1, &["seg/a.zuseg"]), None)
                .unwrap();
            for epoch in 1..3 {
                let next = manifest(epoch, 1, &["seg/a.zuseg", "seg/b.zuseg"]);
                current = store.commit(&next, Some(&current)).unwrap();
                assert_eq!(current.manifest, next);
                assert_eq!(store.read_current().unwrap().unwrap().manifest, next);
            }
            // Every epoch also left an immutable, decodable snapshot behind.
            for epoch in 0..3 {
                let result = block_on(backend.get(&Path::from(snapshot_key(epoch)))).unwrap();
                let bytes = block_on(result.bytes()).unwrap();
                assert_eq!(Manifest::decode(&bytes).unwrap().epoch, epoch);
            }
        });
    }

    #[test]
    fn reopen_from_current_continues_the_chain() {
        each_backend(|backend| {
            let writer = 9;
            {
                let store = ManifestStore::new(backend.clone(), writer);
                let genesis = store.commit(&manifest(0, writer, &[]), None).unwrap();
                store
                    .commit(&manifest(1, writer, &["seg/x.zuseg"]), Some(&genesis))
                    .unwrap();
            }
            let reopened = ManifestStore::new(backend, writer);
            let current = reopened.read_current().unwrap().unwrap();
            assert_eq!(current.manifest.epoch, 1);
            assert_eq!(current.manifest.segments, ["seg/x.zuseg"]);
            reopened
                .commit(&manifest(2, writer, &["seg/x.zuseg"]), Some(&current))
                .unwrap();
        });
    }

    #[test]
    fn genesis_race_exactly_one_writer_wins() {
        each_backend(|backend| {
            let a = ManifestStore::new(backend.clone(), 1);
            let b = ManifestStore::new(backend, 2);
            a.commit(&manifest(0, 1, &[]), None).unwrap();
            let err = b.commit(&manifest(0, 2, &[]), None).unwrap_err();
            assert!(matches!(err, ZuError::Conflict(_)), "{err}");
            assert_eq!(b.read_current().unwrap().unwrap().manifest.writer_id, 1);
        });
    }

    #[test]
    fn stale_cas_token_loses_with_conflict() {
        each_backend(|backend| {
            let store = ManifestStore::new(backend.clone(), 1);
            let stale = store.commit(&manifest(0, 1, &[]), None).unwrap();
            // Refresh CURRENT's version out from under the writer, simulating
            // a concurrent CAS win this writer has not observed yet.
            let bytes = stale.manifest.encode().unwrap();
            block_on(backend.put(&Path::from(CURRENT_KEY), PutPayload::from(bytes))).unwrap();
            let err = store
                .commit(&manifest(1, 1, &[]), Some(&stale))
                .unwrap_err();
            assert!(matches!(err, ZuError::Conflict(_)), "{err}");
        });
    }

    #[test]
    fn foreign_writer_is_fenced_without_takeover() {
        each_backend(|backend| {
            let a = ManifestStore::new(backend.clone(), 1);
            let b = ManifestStore::new(backend, 2);
            a.commit(&manifest(0, 1, &[]), None).unwrap();
            let seen = b.read_current().unwrap().unwrap();
            let err = b.commit(&manifest(1, 2, &[]), Some(&seen)).unwrap_err();
            assert!(matches!(err, ZuError::Conflict(_)), "{err}");
        });
    }

    #[test]
    fn take_over_bumps_epoch_and_fences_the_old_writer() {
        each_backend(|backend| {
            let a = ManifestStore::new(backend.clone(), 1);
            let b = ManifestStore::new(backend, 2);
            let genesis = a.commit(&manifest(0, 1, &["seg/a.zuseg"]), None).unwrap();
            let stale = a
                .commit(&manifest(1, 1, &["seg/a.zuseg"]), Some(&genesis))
                .unwrap();
            let owned = b.take_over().unwrap();
            assert_eq!(owned.manifest.epoch, 2);
            assert_eq!(owned.manifest.writer_id, 2);
            assert_eq!(owned.manifest.segments, ["seg/a.zuseg"]);
            // The old writer's in-hand token now loses the CAS.
            let err = a
                .commit(&manifest(2, 1, &["seg/a.zuseg"]), Some(&stale))
                .unwrap_err();
            assert!(matches!(err, ZuError::Conflict(_)), "{err}");
            // Even after a fresh read it stays fenced until its own take_over.
            let fresh = a.read_current().unwrap().unwrap();
            let err = a.commit(&manifest(3, 1, &[]), Some(&fresh)).unwrap_err();
            assert!(matches!(err, ZuError::Conflict(_)), "{err}");
            // The new writer keeps committing on top.
            let next = manifest(3, 2, &["seg/a.zuseg", "seg/b.zuseg"]);
            b.commit(&next, Some(&owned)).unwrap();
            assert_eq!(b.read_current().unwrap().unwrap().manifest, next);
        });
    }

    #[test]
    fn epoch_must_advance_by_exactly_one() {
        each_backend(|backend| {
            let store = ManifestStore::new(backend, 1);
            let err = store.commit(&manifest(1, 1, &[]), None).unwrap_err();
            assert!(matches!(err, ZuError::InvalidArgument(_)), "{err}");
            let current = store.commit(&manifest(0, 1, &[]), None).unwrap();
            for bad_epoch in [0, 2, 5] {
                let err = store
                    .commit(&manifest(bad_epoch, 1, &[]), Some(&current))
                    .unwrap_err();
                assert!(matches!(err, ZuError::InvalidArgument(_)), "{err}");
            }
            // A manifest carrying someone else's writer_id is a caller bug.
            let err = store
                .commit(&manifest(1, 7, &[]), Some(&current))
                .unwrap_err();
            assert!(matches!(err, ZuError::InvalidArgument(_)), "{err}");
        });
    }

    #[test]
    fn take_over_of_an_empty_store_is_rejected() {
        each_backend(|backend| {
            let store = ManifestStore::new(backend, 1);
            let err = store.take_over().unwrap_err();
            assert!(matches!(err, ZuError::InvalidArgument(_)), "{err}");
        });
    }

    #[test]
    fn corrupt_current_returns_corrupt() {
        each_backend(|backend| {
            block_on(backend.put(&Path::from(CURRENT_KEY), PutPayload::from(vec![1u8, 2, 3])))
                .unwrap();
            let store = ManifestStore::new(backend, 1);
            let err = store.read_current().unwrap_err();
            assert!(matches!(err, ZuError::Corrupt { .. }), "{err}");
        });
    }

    /// LocalFileSystem supports `PutMode::Create` but rejects
    /// `PutMode::Update`, so the commit protocol cannot run on it; this test
    /// pins the limitation that keeps `each_backend` to `InMemory`.
    #[test]
    fn local_fs_lacks_conditional_update() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFileSystem::new_with_prefix(dir.path()).unwrap();
        let path = Path::from(CURRENT_KEY);
        block_on(fs.put_opts(&path, PutPayload::from(vec![0u8]), PutMode::Create.into())).unwrap();
        let err = block_on(fs.put_opts(&path, PutPayload::from(vec![0u8]), PutMode::Create.into()))
            .unwrap_err();
        assert!(matches!(err, object_store::Error::AlreadyExists { .. }));
        let update = PutMode::Update(UpdateVersion {
            e_tag: None,
            version: None,
        });
        let err =
            block_on(fs.put_opts(&path, PutPayload::from(vec![1u8]), update.into())).unwrap_err();
        assert!(matches!(err, object_store::Error::NotImplemented { .. }));
    }
}

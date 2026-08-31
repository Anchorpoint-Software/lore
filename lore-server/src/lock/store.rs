// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use async_trait::async_trait;
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use lore_base::error::InvalidArguments;
use lore_base::error::LockNotFound;
use lore_base::error::LockNotOwned;
use lore_base::types::Hash;
use lore_base::types::LockData;
use lore_base::types::LockResource;
use lore_revision::lock::LockError;
use lore_revision::lock::LockQuery;
use lore_revision::lock::LockStore;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::util;

#[derive(Eq, Hash, PartialEq)]
pub struct LockKey {
    repository: RepositoryId,
    branch: BranchId,
    hash: Hash,
}

#[derive(Default)]
pub struct LocalLockStore {
    storage: DashMap<LockKey, LockData>,
}

#[async_trait]
impl LockStore for LocalLockStore {
    async fn lock_resources(
        &self,
        owner_id: &str,
        repository: RepositoryId,
        resources: &[LockResource],
    ) -> Result<Vec<LockData>, LockError> {
        let mut locks = Vec::<LockData>::with_capacity(resources.len());
        let mut failed = false;
        let timestamp = util::time::timestamp();
        for resource in resources {
            let key = LockKey {
                repository,
                branch: resource.branch,
                hash: resource.hash,
            };

            let lock = LockData {
                resource: resource.clone(),
                owner: owner_id.to_string(),
                locked_at: timestamp,
            };
            // `DashMap::entry` is safe here as it is not held across any awaits and no other locks are acquired while held
            #[allow(clippy::disallowed_methods)]
            match self.storage.entry(key) {
                Entry::Vacant(entry) => entry.insert(lock.clone()),
                Entry::Occupied(entry) => {
                    if entry.get().owner == lock.owner {
                        continue;
                    }
                    failed = true;
                    break;
                }
            };

            locks.push(lock);
        }

        if failed {
            let unlocks: Vec<LockResource> =
                locks.iter().map(|lock| lock.resource.clone()).collect();
            let _ = self
                .unlock_resources(owner_id, true, repository, &unlocks)
                .await;
            return Err(LockError::internal("resource already locked"));
        }

        Ok(locks)
    }

    async fn query_locks(&self, query: LockQuery) -> Result<Vec<LockData>, LockError> {
        let mut locks = Vec::new();

        match query {
            LockQuery::Repository(repository) => {
                for lock in self.storage.iter() {
                    if lock.key().repository == repository {
                        locks.push(lock.value().clone());
                    }
                }
            }
            LockQuery::RepositoryBranch(repository, branch) => {
                for lock in self.storage.iter() {
                    let key = lock.key();
                    let value = lock.value();
                    if key.repository == repository && key.branch == branch {
                        locks.push(value.clone());
                    }
                }
            }
            LockQuery::RepositoryBranchDescription(repository, branch, description) => {
                for lock in self.storage.iter() {
                    let key = lock.key();
                    let value = lock.value();
                    if key.repository == repository
                        && key.branch == branch
                        && value.resource.description == description
                    {
                        locks.push(value.clone());
                    }
                }
            }
            LockQuery::OwnerRepository(owner, repository) => {
                for lock in self.storage.iter() {
                    let key = lock.key();
                    let value = lock.value();
                    if key.repository == repository && value.owner == owner {
                        locks.push(value.clone());
                    }
                }
            }
            LockQuery::OwnerRepositoryBranch(owner, repository, branch) => {
                for lock in self.storage.iter() {
                    let key = lock.key();
                    let value = lock.value();
                    if key.repository == repository && key.branch == branch && value.owner == owner
                    {
                        locks.push(value.clone());
                    }
                }
            }
            LockQuery::HashRepositoryBranch(resource, repository, branch) => {
                let key = LockKey {
                    hash: resource,
                    repository,
                    branch,
                };

                if let Some(lock) = self.storage.get(&key) {
                    locks.push(lock.value().clone());
                }
            }
            _ => {
                return Err(InvalidArguments {
                    reason: "unsupported lock query".into(),
                }
                .into());
            }
        }

        Ok(locks)
    }

    async fn check_locks_status(
        &self,
        repository: RepositoryId,
        resources: &[LockResource],
    ) -> Result<Vec<LockData>, LockError> {
        let mut locked = vec![];

        for resource in resources {
            let key = LockKey {
                repository,
                branch: resource.branch,
                hash: resource.hash,
            };

            if let Some(lock) = self.storage.get(&key) {
                locked.push(lock.value().clone());
            }
        }

        Ok(locked)
    }

    async fn unlock_resources(
        &self,
        owner_id: &str,
        validate_user: bool,
        repository: RepositoryId,
        resources: &[LockResource],
    ) -> Result<Vec<LockResource>, LockError> {
        for resource in resources {
            let key = LockKey {
                repository,
                branch: resource.branch,
                hash: resource.hash,
            };

            // `DashMap::entry` is safe here as it is not held across any awaits and no other locks are acquired while held
            #[allow(clippy::disallowed_methods)]
            match self.storage.entry(key) {
                Entry::Vacant(_) => {
                    return Err(LockNotFound.into());
                }
                Entry::Occupied(entry) => {
                    if validate_user && entry.get().owner != *owner_id {
                        return Err(LockNotOwned.into());
                    }
                    entry.remove();
                }
            }
        }

        Ok(resources.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use lore_revision::lock::util::assemble_resource_for_path;

    use super::*;

    #[tokio::test]
    async fn locking_a_resource_held_by_another_owner_states_the_reason() {
        let store = LocalLockStore::default();
        let repository = RepositoryId::default();
        let resource = assemble_resource_for_path("Map.umap", BranchId::default());

        store
            .lock_resources("user_A", repository, std::slice::from_ref(&resource))
            .await
            .expect("the first owner acquires the lock");

        let error = store
            .lock_resources("user_B", repository, &[resource])
            .await
            .expect_err("a second owner is refused");

        assert_eq!(error.to_string(), "resource already locked");
    }

    #[tokio::test]
    async fn a_refused_batch_keeps_none_of_the_locks_it_took() {
        let store = LocalLockStore::default();
        let repository = RepositoryId::default();
        let branch = BranchId::default();
        let contended = assemble_resource_for_path("Contended.umap", branch);
        let free = assemble_resource_for_path("Free.umap", branch);

        store
            .lock_resources("user_A", repository, std::slice::from_ref(&contended))
            .await
            .expect("the first owner acquires the contended lock");

        store
            .lock_resources("user_B", repository, &[free, contended])
            .await
            .expect_err("the batch is refused");

        let held = store
            .query_locks(LockQuery::Repository(repository))
            .await
            .expect("the held locks can be queried");

        assert_eq!(held.len(), 1, "the refused batch left a lock behind");
        assert_eq!(held[0].owner, "user_A");
    }
}

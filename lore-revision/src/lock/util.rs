// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use crate::hash;
use crate::lock;
use crate::lore::BranchId;

pub const LOCK_BATCH_SIZE: usize = 100;

pub fn assemble_resource_for_path(path: &str, branch: BranchId) -> lock::LockResource {
    let hash = hash::hash_slice(path.as_bytes());
    let description = path.to_string();
    lock::LockResource {
        branch,
        hash,
        description,
    }
}

/// Folds per-batch outcomes into the collected items, the count of successful
/// batches, and the error of one failing batch, which carries the reason the
/// remote refused it. Batches complete out of order, so which failing batch the
/// error comes from is unspecified. `capacity` sizes the item vector up front
/// and is an upper bound on what the batches return.
pub fn fold_batch_results<T, E>(
    batch_results: Vec<Result<Vec<T>, E>>,
    capacity: usize,
) -> (Vec<T>, usize, Option<E>) {
    let mut items = Vec::with_capacity(capacity);
    let mut num_success = 0;
    let mut first_error = None;
    for batch_result in batch_results {
        match batch_result {
            Ok(mut batch_items) => {
                items.append(&mut batch_items);
                num_success += 1;
            }
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }

    (items, num_success, first_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_batch_results_collects_every_batch() {
        let (items, num_success, first_error) =
            fold_batch_results::<u32, String>(vec![Ok(vec![1, 2]), Ok(vec![]), Ok(vec![3])], 3);

        assert_eq!(items, vec![1, 2, 3]);
        assert_eq!(num_success, 3);
        assert!(first_error.is_none());
    }

    #[test]
    fn fold_batch_results_reserves_the_requested_capacity() {
        let (items, _, _) = fold_batch_results::<u32, String>(vec![Ok(vec![1])], 64);

        assert!(
            items.capacity() >= 64,
            "capacity {} was not reserved up front",
            items.capacity()
        );
    }

    #[test]
    fn fold_batch_results_keeps_one_error_and_the_successful_items() {
        let (items, num_success, first_error) = fold_batch_results::<u32, String>(
            vec![
                Ok(vec![1]),
                Err("first".to_string()),
                Err("second".to_string()),
                Ok(vec![2]),
            ],
            2,
        );

        assert_eq!(items, vec![1, 2]);
        assert_eq!(num_success, 2);
        assert_eq!(first_error, Some("first".to_string()));
    }

    #[test]
    fn fold_batch_results_reports_no_error_without_batches() {
        let (items, num_success, first_error) = fold_batch_results::<u32, String>(vec![], 0);

        assert!(items.is_empty());
        assert_eq!(num_success, 0);
        assert!(first_error.is_none());
    }
}

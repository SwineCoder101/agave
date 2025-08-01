//! Service to calculate accounts hashes

use {
    crate::snapshot_packager_service::PendingSnapshotPackages,
    crossbeam_channel::{Receiver, Sender},
    solana_accounts_db::{
        accounts_db::CalcAccountsHashKind,
        accounts_hash::{
            AccountsHash, CalcAccountsHashConfig, HashStats, IncrementalAccountsHash,
            MerkleOrLatticeAccountsHash,
        },
        sorted_storages::SortedStorages,
    },
    solana_clock::{Slot, DEFAULT_MS_PER_SLOT},
    solana_measure::measure_us,
    solana_runtime::{
        serde_snapshot::BankIncrementalSnapshotPersistence,
        snapshot_config::SnapshotConfig,
        snapshot_controller::SnapshotController,
        snapshot_package::{
            self, AccountsHashAlgorithm, AccountsPackage, AccountsPackageKind, SnapshotKind,
            SnapshotPackage,
        },
        snapshot_utils,
    },
    std::{
        io,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex,
        },
        thread::{self, Builder, JoinHandle},
        time::Duration,
    },
};

pub struct AccountsHashVerifier {
    t_accounts_hash_verifier: JoinHandle<()>,
}

impl AccountsHashVerifier {
    pub fn new(
        accounts_package_sender: Sender<AccountsPackage>,
        accounts_package_receiver: Receiver<AccountsPackage>,
        pending_snapshot_packages: Arc<Mutex<PendingSnapshotPackages>>,
        exit: Arc<AtomicBool>,
        snapshot_controller: Arc<SnapshotController>,
    ) -> Self {
        // If there are no accounts packages to process, limit how often we re-check
        const LOOP_LIMITER: Duration = Duration::from_millis(DEFAULT_MS_PER_SLOT);
        let t_accounts_hash_verifier = Builder::new()
            .name("solAcctHashVer".to_string())
            .spawn(move || {
                info!("AccountsHashVerifier has started");
                loop {
                    if exit.load(Ordering::Relaxed) {
                        break;
                    }

                    let Some((
                        accounts_package,
                        num_outstanding_accounts_packages,
                        num_re_enqueued_accounts_packages,
                    )) = Self::get_next_accounts_package(
                        &accounts_package_sender,
                        &accounts_package_receiver,
                    )
                    else {
                        std::thread::sleep(LOOP_LIMITER);
                        continue;
                    };
                    info!("handling accounts package: {accounts_package:?}");
                    let enqueued_time = accounts_package.enqueued.elapsed();

                    let snapshot_config = snapshot_controller.snapshot_config();
                    let (result, handling_time_us) = measure_us!(Self::process_accounts_package(
                        accounts_package,
                        &pending_snapshot_packages,
                        snapshot_config,
                    ));
                    if let Err(err) = result {
                        error!(
                            "Stopping AccountsHashVerifier! Fatal error while processing accounts \
                             package: {err}"
                        );
                        exit.store(true, Ordering::Relaxed);
                        break;
                    }

                    datapoint_info!(
                        "accounts_hash_verifier",
                        (
                            "num_outstanding_accounts_packages",
                            num_outstanding_accounts_packages,
                            i64
                        ),
                        (
                            "num_re_enqueued_accounts_packages",
                            num_re_enqueued_accounts_packages,
                            i64
                        ),
                        ("enqueued_time_us", enqueued_time.as_micros(), i64),
                        ("handling_time_us", handling_time_us, i64),
                    );
                }
                info!("AccountsHashVerifier has stopped");
            })
            .unwrap();
        Self {
            t_accounts_hash_verifier,
        }
    }

    /// Get the next accounts package to handle
    ///
    /// Look through the accounts package channel to find the highest priority one to handle next.
    /// If there are no accounts packages in the channel, return None.  Otherwise return the
    /// highest priority one.  Unhandled accounts packages with slots GREATER-THAN the handled one
    /// will be re-enqueued.  The remaining will be dropped.
    ///
    /// Also return the number of accounts packages initially in the channel, and the number of
    /// ones re-enqueued.
    fn get_next_accounts_package(
        accounts_package_sender: &Sender<AccountsPackage>,
        accounts_package_receiver: &Receiver<AccountsPackage>,
    ) -> Option<(
        AccountsPackage,
        /*num outstanding accounts packages*/ usize,
        /*num re-enqueued accounts packages*/ usize,
    )> {
        let mut accounts_packages: Vec<_> = accounts_package_receiver.try_iter().collect();
        let accounts_packages_len = accounts_packages.len();
        debug!("outstanding accounts packages ({accounts_packages_len}): {accounts_packages:?}");

        // NOTE: This code to select the next request is mirrored in AccountsBackgroundService.
        // Please ensure they stay in sync.
        match accounts_packages_len {
            0 => None,
            1 => {
                // SAFETY: We know the len is 1, so `pop` will return `Some`
                let accounts_package = accounts_packages.pop().unwrap();
                Some((accounts_package, 1, 0))
            }
            _ => {
                let num_eah_packages = accounts_packages
                    .iter()
                    .filter(|account_package| {
                        account_package.package_kind == AccountsPackageKind::EpochAccountsHash
                    })
                    .count();
                assert!(
                    num_eah_packages <= 1,
                    "Only a single EAH accounts package is allowed at a time! count: \
                     {num_eah_packages}"
                );

                // Get the two highest priority requests, `y` and `z`.
                // By asking for the second-to-last element to be in its final sorted position, we
                // also ensure that the last element is also sorted.
                let (_, y, z) = accounts_packages.select_nth_unstable_by(
                    accounts_packages_len - 2,
                    snapshot_package::cmp_accounts_packages_by_priority,
                );
                assert_eq!(z.len(), 1);
                let z = z.first().unwrap();
                let y: &_ = y; // reborrow to remove `mut`

                // If the highest priority request (`z`) is EpochAccountsHash, we need to check if
                // there's a FullSnapshot request with a lower slot in `y` that is about to be
                // dropped.  We do not want to drop a FullSnapshot request in this case because it
                // will cause subsequent IncrementalSnapshot requests to fail.
                //
                // So, if `z` is an EpochAccountsHash request, check `y`.  We know there can only
                // be at most one EpochAccountsHash request, so `y` is the only other request we
                // need to check.  If `y` is a FullSnapshot request *with a lower slot* than `z`,
                // then handle `y` first.
                let accounts_package = if z.package_kind == AccountsPackageKind::EpochAccountsHash
                    && y.package_kind == AccountsPackageKind::Snapshot(SnapshotKind::FullSnapshot)
                    && y.slot < z.slot
                {
                    // SAFETY: We know the len is > 1, so both `pop`s will return `Some`
                    let z = accounts_packages.pop().unwrap();
                    let y = accounts_packages.pop().unwrap();
                    accounts_packages.push(z);
                    y
                } else {
                    // SAFETY: We know the len is > 1, so `pop` will return `Some`
                    accounts_packages.pop().unwrap()
                };

                let handled_accounts_package_slot = accounts_package.slot;
                // re-enqueue any remaining accounts packages for slots GREATER-THAN the accounts package
                // that will be handled
                let num_re_enqueued_accounts_packages = accounts_packages
                    .into_iter()
                    .filter(|accounts_package| {
                        accounts_package.slot > handled_accounts_package_slot
                    })
                    .map(|accounts_package| {
                        accounts_package_sender
                            .try_send(accounts_package)
                            .expect("re-enqueue accounts package")
                    })
                    .count();

                Some((
                    accounts_package,
                    accounts_packages_len,
                    num_re_enqueued_accounts_packages,
                ))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn process_accounts_package(
        accounts_package: AccountsPackage,
        pending_snapshot_packages: &Mutex<PendingSnapshotPackages>,
        snapshot_config: &SnapshotConfig,
    ) -> io::Result<()> {
        let (merkle_or_lattice_accounts_hash, bank_incremental_snapshot_persistence) =
            Self::calculate_and_verify_accounts_hash(&accounts_package, snapshot_config)?;

        Self::purge_old_accounts_hashes(&accounts_package, snapshot_config);

        Self::submit_for_packaging(
            accounts_package,
            pending_snapshot_packages,
            merkle_or_lattice_accounts_hash,
            bank_incremental_snapshot_persistence,
        );

        Ok(())
    }

    /// returns calculated accounts hash
    fn calculate_and_verify_accounts_hash(
        accounts_package: &AccountsPackage,
        snapshot_config: &SnapshotConfig,
    ) -> io::Result<(
        MerkleOrLatticeAccountsHash,
        Option<BankIncrementalSnapshotPersistence>,
    )> {
        match accounts_package.accounts_hash_algorithm {
            AccountsHashAlgorithm::Merkle => {
                debug!(
                    "calculate_and_verify_accounts_hash(): snapshots lt hash is disabled, DO \
                     merkle-based accounts hash calculation",
                );
            }
            AccountsHashAlgorithm::Lattice => {
                debug!(
                    "calculate_and_verify_accounts_hash(): snapshots lt hash is enabled, SKIP \
                     merkle-based accounts hash calculation",
                );
                return Ok((MerkleOrLatticeAccountsHash::Lattice, None));
            }
        }

        let accounts_hash_calculation_kind = match accounts_package.package_kind {
            AccountsPackageKind::EpochAccountsHash => unreachable!("EAH is removed"),
            AccountsPackageKind::Snapshot(snapshot_kind) => match snapshot_kind {
                SnapshotKind::FullSnapshot => CalcAccountsHashKind::Full,
                SnapshotKind::IncrementalSnapshot(_) => CalcAccountsHashKind::Incremental,
            },
        };

        let (accounts_hash_kind, bank_incremental_snapshot_persistence) =
            match accounts_hash_calculation_kind {
                CalcAccountsHashKind::Full => {
                    let (accounts_hash, _capitalization) =
                        Self::_calculate_full_accounts_hash(accounts_package);
                    (accounts_hash.into(), None)
                }
                CalcAccountsHashKind::Incremental => {
                    let AccountsPackageKind::Snapshot(SnapshotKind::IncrementalSnapshot(base_slot)) =
                        accounts_package.package_kind
                    else {
                        panic!("Calculating incremental accounts hash requires a base slot");
                    };
                    let accounts_db = &accounts_package.accounts.accounts_db;
                    let Some((base_accounts_hash, base_capitalization)) =
                        accounts_db.get_accounts_hash(base_slot)
                    else {
                        #[rustfmt::skip]
                        panic!(
                            "incremental snapshot requires accounts hash and capitalization from \
                             the full snapshot it is based on\n\
                             package: {accounts_package:?}\n\
                             accounts hashes: {:?}\n\
                             incremental accounts hashes: {:?}\n\
                             full snapshot archives: {:?}\n\
                             bank snapshots: {:?}",
                            accounts_db.get_accounts_hashes(),
                            accounts_db.get_incremental_accounts_hashes(),
                            snapshot_utils::get_full_snapshot_archives(
                                &snapshot_config.full_snapshot_archives_dir,
                            ),
                            snapshot_utils::get_bank_snapshots(&snapshot_config.bank_snapshots_dir),
                        );
                    };
                    let (incremental_accounts_hash, incremental_capitalization) =
                        Self::_calculate_incremental_accounts_hash(accounts_package, base_slot);
                    let bank_incremental_snapshot_persistence =
                        BankIncrementalSnapshotPersistence {
                            full_slot: base_slot,
                            full_hash: base_accounts_hash.into(),
                            full_capitalization: base_capitalization,
                            incremental_hash: incremental_accounts_hash.into(),
                            incremental_capitalization,
                        };
                    (
                        incremental_accounts_hash.into(),
                        Some(bank_incremental_snapshot_persistence),
                    )
                }
            };

        Ok((
            MerkleOrLatticeAccountsHash::Merkle(accounts_hash_kind),
            bank_incremental_snapshot_persistence,
        ))
    }

    fn _calculate_full_accounts_hash(
        accounts_package: &AccountsPackage,
    ) -> (AccountsHash, /*capitalization*/ u64) {
        let (sorted_storages, storage_sort_us) =
            measure_us!(SortedStorages::new(&accounts_package.snapshot_storages));

        let mut timings = HashStats {
            storage_sort_us,
            ..HashStats::default()
        };
        timings.calc_storage_size_quartiles(&accounts_package.snapshot_storages);

        let epoch = accounts_package
            .epoch_schedule
            .get_epoch(accounts_package.slot);
        let calculate_accounts_hash_config = CalcAccountsHashConfig {
            use_bg_thread_pool: true,
            ancestors: None,
            epoch_schedule: &accounts_package.epoch_schedule,
            epoch,
            store_detailed_debug_info_on_failure: false,
        };

        let slot = accounts_package.slot;
        let ((accounts_hash, lamports), measure_hash_us) =
            measure_us!(accounts_package.accounts.accounts_db.update_accounts_hash(
                &calculate_accounts_hash_config,
                &sorted_storages,
                slot,
                timings,
            ));

        if accounts_package.expected_capitalization != lamports {
            // before we assert, run the hash calc again. This helps track down whether it could have been a failure in a race condition possibly with shrink.
            // We could add diagnostics to the hash calc here to produce a per bin cap or something to help narrow down how many pubkeys are different.
            let calculate_accounts_hash_config = CalcAccountsHashConfig {
                // since we're going to assert, use the fg thread pool to go faster
                use_bg_thread_pool: false,
                // now that we've failed, store off the failing contents that produced a bad capitalization
                store_detailed_debug_info_on_failure: true,
                ..calculate_accounts_hash_config
            };
            let second_accounts_hash = accounts_package
                .accounts
                .accounts_db
                .calculate_accounts_hash(
                    &calculate_accounts_hash_config,
                    &sorted_storages,
                    HashStats::default(),
                );
            panic!(
                "accounts hash capitalization mismatch: expected {}, but calculated {} (then \
                 recalculated {})",
                accounts_package.expected_capitalization, lamports, second_accounts_hash.1,
            );
        }

        if let Some(expected_hash) = accounts_package.accounts_hash_for_testing {
            assert_eq!(expected_hash, accounts_hash);
        };

        datapoint_info!(
            "accounts_hash_verifier",
            ("calculate_hash", measure_hash_us, i64),
        );

        (accounts_hash, lamports)
    }

    fn _calculate_incremental_accounts_hash(
        accounts_package: &AccountsPackage,
        base_slot: Slot,
    ) -> (IncrementalAccountsHash, /*capitalization*/ u64) {
        let incremental_storages =
            accounts_package
                .snapshot_storages
                .iter()
                .filter_map(|storage| {
                    let storage_slot = storage.slot();
                    (storage_slot > base_slot).then_some((storage, storage_slot))
                });
        let sorted_storages = SortedStorages::new_with_slots(incremental_storages, None, None);

        let epoch = accounts_package
            .epoch_schedule
            .get_epoch(accounts_package.slot);
        let calculate_accounts_hash_config = CalcAccountsHashConfig {
            use_bg_thread_pool: true,
            ancestors: None,
            epoch_schedule: &accounts_package.epoch_schedule,
            epoch,
            store_detailed_debug_info_on_failure: false,
        };

        let (incremental_accounts_hash, measure_hash_us) = measure_us!(accounts_package
            .accounts
            .accounts_db
            .update_incremental_accounts_hash(
                &calculate_accounts_hash_config,
                &sorted_storages,
                accounts_package.slot,
                HashStats::default(),
            ));

        datapoint_info!(
            "accounts_hash_verifier",
            (
                "calculate_incremental_accounts_hash_us",
                measure_hash_us,
                i64
            ),
        );

        incremental_accounts_hash
    }

    fn purge_old_accounts_hashes(
        accounts_package: &AccountsPackage,
        snapshot_config: &SnapshotConfig,
    ) {
        let should_purge = match (
            snapshot_config.should_generate_snapshots(),
            accounts_package.package_kind,
        ) {
            (false, _) => {
                // If we are *not* generating snapshots, then it is safe to purge every time.
                true
            }
            (true, AccountsPackageKind::Snapshot(SnapshotKind::FullSnapshot)) => {
                // If we *are* generating snapshots, then only purge old accounts hashes after
                // handling full snapshot packages.  This is because handling incremental snapshot
                // packages requires the accounts hash from the latest full snapshot, and if we
                // purged after every package, we'd remove the accounts hash needed by the next
                // incremental snapshot.
                true
            }
            (true, _) => false,
        };

        if should_purge {
            accounts_package
                .accounts
                .accounts_db
                .purge_old_accounts_hashes(accounts_package.slot);
        }
    }

    fn submit_for_packaging(
        accounts_package: AccountsPackage,
        pending_snapshot_packages: &Mutex<PendingSnapshotPackages>,
        merkle_or_lattice_accounts_hash: MerkleOrLatticeAccountsHash,
        bank_incremental_snapshot_persistence: Option<BankIncrementalSnapshotPersistence>,
    ) {
        if !matches!(
            accounts_package.package_kind,
            AccountsPackageKind::Snapshot(_)
        ) {
            return;
        }

        let snapshot_package = SnapshotPackage::new(
            accounts_package,
            merkle_or_lattice_accounts_hash,
            bank_incremental_snapshot_persistence,
        );
        pending_snapshot_packages
            .lock()
            .unwrap()
            .push(snapshot_package);
    }

    pub fn join(self) -> thread::Result<()> {
        self.t_accounts_hash_verifier.join()
    }
}

#[cfg(test)]
mod tests {
    use {super::*, rand::seq::SliceRandom, solana_runtime::snapshot_package::SnapshotKind};

    fn new(package_kind: AccountsPackageKind, slot: Slot) -> AccountsPackage {
        AccountsPackage {
            package_kind,
            slot,
            block_height: slot,
            ..AccountsPackage::default_for_tests()
        }
    }
    fn new_eah(slot: Slot) -> AccountsPackage {
        new(AccountsPackageKind::EpochAccountsHash, slot)
    }
    fn new_fss(slot: Slot) -> AccountsPackage {
        new(
            AccountsPackageKind::Snapshot(SnapshotKind::FullSnapshot),
            slot,
        )
    }
    fn new_iss(slot: Slot, base: Slot) -> AccountsPackage {
        new(
            AccountsPackageKind::Snapshot(SnapshotKind::IncrementalSnapshot(base)),
            slot,
        )
    }

    /// Ensure that unhandled accounts packages are properly re-enqueued or dropped
    ///
    /// The accounts package handler should re-enqueue unhandled accounts packages, if those
    /// unhandled accounts packages are for slots GREATER-THAN the last handled accounts package.
    /// Otherwise, they should be dropped.
    #[test]
    fn test_get_next_accounts_package1() {
        let (accounts_package_sender, accounts_package_receiver) = crossbeam_channel::unbounded();

        // Populate the channel so that re-enqueueing and dropping will be tested
        let mut accounts_packages = [
            new_fss(100), // skipped, since there's another full snapshot with a higher slot
            new_iss(110, 100),
            new_eah(200), // <-- handle 1st
            new_iss(210, 100),
            new_fss(300),
            new_iss(310, 300),
            new_fss(400), // <-- handle 2nd
            new_iss(410, 400),
            new_iss(420, 400), // <-- handle 3rd
        ];
        // Shuffle the accounts packages to simulate receiving new accounts packages from ABS
        // simultaneously as AHV is processing them.
        accounts_packages.shuffle(&mut rand::thread_rng());
        accounts_packages
            .into_iter()
            .for_each(|accounts_package| accounts_package_sender.send(accounts_package).unwrap());

        // The EAH is handled 1st
        let (
            account_package,
            _num_outstanding_accounts_packages,
            num_re_enqueued_accounts_packages,
        ) = AccountsHashVerifier::get_next_accounts_package(
            &accounts_package_sender,
            &accounts_package_receiver,
        )
        .unwrap();
        assert_eq!(
            account_package.package_kind,
            AccountsPackageKind::EpochAccountsHash
        );
        assert_eq!(account_package.slot, 200);
        assert_eq!(num_re_enqueued_accounts_packages, 6);

        // The Full Snapshot from slot 400 is handled 2nd
        // (the older full snapshot from slot 300 is skipped and dropped)
        let (
            account_package,
            _num_outstanding_accounts_packages,
            num_re_enqueued_accounts_packages,
        ) = AccountsHashVerifier::get_next_accounts_package(
            &accounts_package_sender,
            &accounts_package_receiver,
        )
        .unwrap();
        assert_eq!(
            account_package.package_kind,
            AccountsPackageKind::Snapshot(SnapshotKind::FullSnapshot)
        );
        assert_eq!(account_package.slot, 400);
        assert_eq!(num_re_enqueued_accounts_packages, 2);

        // The Incremental Snapshot from slot 420 is handled 3rd
        // (the older incremental snapshot from slot 410 is skipped and dropped)
        let (
            account_package,
            _num_outstanding_accounts_packages,
            num_re_enqueued_accounts_packages,
        ) = AccountsHashVerifier::get_next_accounts_package(
            &accounts_package_sender,
            &accounts_package_receiver,
        )
        .unwrap();
        assert_eq!(
            account_package.package_kind,
            AccountsPackageKind::Snapshot(SnapshotKind::IncrementalSnapshot(400))
        );
        assert_eq!(account_package.slot, 420);
        assert_eq!(num_re_enqueued_accounts_packages, 0);

        // And now the accounts package channel is empty!
        assert!(AccountsHashVerifier::get_next_accounts_package(
            &accounts_package_sender,
            &accounts_package_receiver
        )
        .is_none());
    }

    /// Ensure that unhandled accounts packages are properly re-enqueued or dropped
    ///
    /// This test differs from the one above by having an older full snapshot request that must be
    /// handled before the new epoch accounts hash request.
    #[test]
    fn test_get_next_accounts_package2() {
        let (accounts_package_sender, accounts_package_receiver) = crossbeam_channel::unbounded();

        // Populate the channel so that re-enqueueing and dropping will be tested
        let mut accounts_packages = [
            new_fss(100), // <-- handle 1st
            new_iss(110, 100),
            new_eah(200), // <-- handle 2nd
            new_iss(210, 100),
            new_iss(220, 100), // <-- handle 3rd
        ];
        // Shuffle the accounts packages to simulate receiving new accounts packages from ABS
        // simultaneously as AHV is processing them.
        accounts_packages.shuffle(&mut rand::thread_rng());
        accounts_packages
            .into_iter()
            .for_each(|accounts_package| accounts_package_sender.send(accounts_package).unwrap());

        // The Full Snapshot is handled 1st
        let (
            account_package,
            _num_outstanding_accounts_packages,
            num_re_enqueued_accounts_packages,
        ) = AccountsHashVerifier::get_next_accounts_package(
            &accounts_package_sender,
            &accounts_package_receiver,
        )
        .unwrap();
        assert_eq!(
            account_package.package_kind,
            AccountsPackageKind::Snapshot(SnapshotKind::FullSnapshot)
        );
        assert_eq!(account_package.slot, 100);
        assert_eq!(num_re_enqueued_accounts_packages, 4);

        // The EAH is handled 2nd
        let (
            account_package,
            _num_outstanding_accounts_packages,
            num_re_enqueued_accounts_packages,
        ) = AccountsHashVerifier::get_next_accounts_package(
            &accounts_package_sender,
            &accounts_package_receiver,
        )
        .unwrap();
        assert_eq!(
            account_package.package_kind,
            AccountsPackageKind::EpochAccountsHash
        );
        assert_eq!(account_package.slot, 200);
        assert_eq!(num_re_enqueued_accounts_packages, 2);

        // The Incremental Snapshot from slot 220 is handled 3rd
        // (the older incremental snapshot from slot 210 is skipped and dropped)
        let (
            account_package,
            _num_outstanding_accounts_packages,
            num_re_enqueued_accounts_packages,
        ) = AccountsHashVerifier::get_next_accounts_package(
            &accounts_package_sender,
            &accounts_package_receiver,
        )
        .unwrap();
        assert_eq!(
            account_package.package_kind,
            AccountsPackageKind::Snapshot(SnapshotKind::IncrementalSnapshot(100))
        );
        assert_eq!(account_package.slot, 220);
        assert_eq!(num_re_enqueued_accounts_packages, 0);

        // And now the accounts package channel is empty!
        assert!(AccountsHashVerifier::get_next_accounts_package(
            &accounts_package_sender,
            &accounts_package_receiver
        )
        .is_none());
    }
}

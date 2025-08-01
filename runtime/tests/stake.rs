#![allow(clippy::arithmetic_side_effects)]

use {
    solana_account::{from_account, state_traits::StateMut},
    solana_client_traits::SyncClient,
    solana_clock::Slot,
    solana_epoch_schedule::{EpochSchedule, MINIMUM_SLOTS_PER_EPOCH},
    solana_keypair::Keypair,
    solana_message::Message,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_runtime::{
        bank::Bank,
        bank_client::BankClient,
        bank_forks::BankForks,
        genesis_utils::{create_genesis_config_with_leader, GenesisConfigInfo},
    },
    solana_signer::Signer,
    solana_stake_interface::{
        self as stake, instruction as stake_instruction,
        state::{Authorized, Lockup, StakeStateV2},
    },
    solana_stake_program::stake_state,
    solana_sysvar::{self as sysvar, stake_history::StakeHistory},
    solana_vote_program::{
        vote_instruction,
        vote_state::{TowerSync, VoteInit, VoteStateV3, VoteStateVersions, MAX_LOCKOUT_HISTORY},
    },
    std::sync::{Arc, RwLock},
};

fn new_bank_from_parent_with_bank_forks(
    bank_forks: &RwLock<BankForks>,
    parent: Arc<Bank>,
    collector_id: &Pubkey,
    slot: Slot,
) -> Arc<Bank> {
    let bank = Bank::new_from_parent(parent, collector_id, slot);
    bank_forks
        .write()
        .unwrap()
        .insert(bank)
        .clone_without_scheduler()
}

/// get bank at next epoch + `n` slots
fn next_epoch_and_n_slots(
    bank: Arc<Bank>,
    bank_forks: &RwLock<BankForks>,
    mut n: usize,
) -> Arc<Bank> {
    bank.squash();
    let slot = bank.get_slots_in_epoch(bank.epoch()) + bank.slot();
    let mut bank = new_bank_from_parent_with_bank_forks(bank_forks, bank, &Pubkey::default(), slot);

    while n > 0 {
        bank.squash();
        let slot = bank.slot() + 1;
        bank = new_bank_from_parent_with_bank_forks(bank_forks, bank, &Pubkey::default(), slot);
        n -= 1;
    }

    bank
}

fn fill_epoch_with_votes(
    mut bank: Arc<Bank>,
    bank_forks: &RwLock<BankForks>,
    vote_keypair: &Keypair,
    mint_keypair: &Keypair,
    start_slot: Slot,
) -> Arc<Bank> {
    let mint_pubkey = mint_keypair.pubkey();
    let vote_pubkey = vote_keypair.pubkey();
    let old_epoch = bank.epoch();
    while bank.epoch() != old_epoch + 1 {
        bank.squash();
        let slot = bank.slot() + 1;
        bank = new_bank_from_parent_with_bank_forks(bank_forks, bank, &Pubkey::default(), slot);

        let bank_client = BankClient::new_shared(bank.clone());
        let parent = bank.parent().unwrap();
        let lowest_slot = u64::max(
            (parent.slot() + 1).saturating_sub(MAX_LOCKOUT_HISTORY as u64),
            start_slot,
        );
        let slots: Vec<_> = (lowest_slot..(parent.slot() + 1)).collect();
        let root = (lowest_slot > start_slot).then(|| lowest_slot - 1);
        let tower_sync = TowerSync::new_from_slots(slots, parent.hash(), root);
        let message = Message::new(
            &[vote_instruction::tower_sync(
                &vote_pubkey,
                &vote_pubkey,
                tower_sync,
            )],
            Some(&mint_pubkey),
        );
        assert!(bank_client
            .send_and_confirm_message(&[mint_keypair, vote_keypair], message)
            .is_ok());
    }
    bank
}

fn warmed_up(bank: &Bank, stake_pubkey: &Pubkey) -> bool {
    let stake = stake_state::stake_from(&bank.get_account(stake_pubkey).unwrap()).unwrap();

    stake.delegation.stake
        == stake.stake(
            bank.epoch(),
            &from_account::<StakeHistory, _>(
                &bank.get_account(&sysvar::stake_history::id()).unwrap(),
            )
            .unwrap(),
            bank.new_warmup_cooldown_rate_epoch(),
        )
}

fn get_staked(bank: &Bank, stake_pubkey: &Pubkey) -> u64 {
    stake_state::stake_from(&bank.get_account(stake_pubkey).unwrap())
        .unwrap()
        .stake(
            bank.epoch(),
            &from_account::<StakeHistory, _>(
                &bank.get_account(&sysvar::stake_history::id()).unwrap(),
            )
            .unwrap(),
            bank.new_warmup_cooldown_rate_epoch(),
        )
}

#[test]
fn test_stake_create_and_split_single_signature() {
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair: staker_keypair,
        ..
    } = create_genesis_config_with_leader(100_000_000_000, &solana_pubkey::new_rand(), 1_000_000);

    let staker_pubkey = staker_keypair.pubkey();

    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let bank_client = BankClient::new_shared(bank.clone());

    let stake_address =
        Pubkey::create_with_seed(&staker_pubkey, "stake", &stake::program::id()).unwrap();

    let authorized = Authorized::auto(&staker_pubkey);

    let lamports = {
        let rent = &bank.rent_collector().rent;
        let rent_exempt_reserve = rent.minimum_balance(StakeStateV2::size_of());
        let minimum_delegation = solana_stake_program::get_minimum_delegation(
            bank.feature_set
                .is_active(&agave_feature_set::stake_raise_minimum_delegation_to_1_sol::id()),
        );
        2 * (rent_exempt_reserve + minimum_delegation)
    };

    // Create stake account with seed
    let message = Message::new(
        &stake_instruction::create_account_with_seed(
            &staker_pubkey, // from
            &stake_address, // to
            &staker_pubkey, // base
            "stake",        // seed
            &authorized,
            &Lockup::default(),
            lamports,
        ),
        Some(&staker_pubkey),
    );

    // only one signature required
    bank_client
        .send_and_confirm_message(&[&staker_keypair], message)
        .expect("failed to create and delegate stake account");

    // split the stake
    let split_stake_address =
        Pubkey::create_with_seed(&staker_pubkey, "split_stake", &stake::program::id()).unwrap();
    // Test split
    let message = Message::new(
        &stake_instruction::split_with_seed(
            &stake_address, // original
            &staker_pubkey, // authorized
            lamports / 2,
            &split_stake_address, // new address
            &staker_pubkey,       // base
            "split_stake",        // seed
        ),
        Some(&staker_keypair.pubkey()),
    );

    assert!(bank_client
        .send_and_confirm_message(&[&staker_keypair], message)
        .is_ok());

    // w00t!
}

#[test]
fn test_stake_create_and_split_to_existing_system_account() {
    // Ensure stake-split allows the user to promote an existing system account into
    // a stake account.

    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair: staker_keypair,
        ..
    } = create_genesis_config_with_leader(100_000_000_000, &solana_pubkey::new_rand(), 1_000_000);

    let staker_pubkey = staker_keypair.pubkey();

    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let bank_client = BankClient::new_shared(bank.clone());

    let stake_address =
        Pubkey::create_with_seed(&staker_pubkey, "stake", &stake::program::id()).unwrap();

    let authorized = Authorized::auto(&staker_pubkey);

    let lamports = {
        let rent = &bank.rent_collector().rent;
        let rent_exempt_reserve = rent.minimum_balance(StakeStateV2::size_of());
        let minimum_delegation = solana_stake_program::get_minimum_delegation(
            bank.feature_set
                .is_active(&agave_feature_set::stake_raise_minimum_delegation_to_1_sol::id()),
        );
        2 * (rent_exempt_reserve + minimum_delegation)
    };

    // Create stake account with seed
    let message = Message::new(
        &stake_instruction::create_account_with_seed(
            &staker_pubkey, // from
            &stake_address, // to
            &staker_pubkey, // base
            "stake",        // seed
            &authorized,
            &Lockup::default(),
            lamports,
        ),
        Some(&staker_pubkey),
    );

    bank_client
        .send_and_confirm_message(&[&staker_keypair], message)
        .expect("failed to create and delegate stake account");

    let split_stake_address =
        Pubkey::create_with_seed(&staker_pubkey, "split_stake", &stake::program::id()).unwrap();

    // First, put a system account where we want the new stake account
    let existing_lamports = 42;
    bank_client
        .transfer_and_confirm(existing_lamports, &staker_keypair, &split_stake_address)
        .unwrap();
    assert_eq!(
        bank_client.get_balance(&split_stake_address).unwrap(),
        existing_lamports
    );

    // Verify the split succeeds with lamports in the destination account
    let message = Message::new(
        &stake_instruction::split_with_seed(
            &stake_address, // original
            &staker_pubkey, // authorized
            lamports / 2,
            &split_stake_address, // new address
            &staker_pubkey,       // base
            "split_stake",        // seed
        ),
        Some(&staker_keypair.pubkey()),
    );
    bank_client
        .send_and_confirm_message(&[&staker_keypair], message)
        .expect("failed to split into account with lamports");
    assert_eq!(
        bank_client.get_balance(&split_stake_address).unwrap(),
        existing_lamports + lamports / 2
    );
}

#[test]
fn test_stake_account_lifetime() {
    let stake_keypair = Keypair::new();
    let stake_pubkey = stake_keypair.pubkey();
    let vote_keypair = Keypair::new();
    let vote_pubkey = vote_keypair.pubkey();
    let identity_keypair = Keypair::new();
    let identity_pubkey = identity_keypair.pubkey();

    let GenesisConfigInfo {
        mut genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config_with_leader(
        100_000_000_000,
        &solana_pubkey::new_rand(),
        2_000_000_000,
    );
    genesis_config.epoch_schedule = EpochSchedule::new(MINIMUM_SLOTS_PER_EPOCH);
    genesis_config.rent = Rent::default();
    let (mut bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let mint_pubkey = mint_keypair.pubkey();
    let bank_client = BankClient::new_shared(bank.clone());

    let (vote_balance, stake_rent_exempt_reserve, stake_minimum_delegation) = {
        let rent = &bank.rent_collector().rent;
        (
            rent.minimum_balance(VoteStateV3::size_of()),
            rent.minimum_balance(StakeStateV2::size_of()),
            solana_stake_program::get_minimum_delegation(
                bank.feature_set
                    .is_active(&agave_feature_set::stake_raise_minimum_delegation_to_1_sol::id()),
            ),
        )
    };

    // Create Vote Account
    let message = Message::new(
        &vote_instruction::create_account_with_config(
            &mint_pubkey,
            &vote_pubkey,
            &VoteInit {
                node_pubkey: identity_pubkey,
                authorized_voter: vote_pubkey,
                authorized_withdrawer: vote_pubkey,
                commission: 50,
            },
            vote_balance,
            vote_instruction::CreateVoteAccountConfig {
                space: VoteStateVersions::vote_state_size_of(true) as u64,
                ..vote_instruction::CreateVoteAccountConfig::default()
            },
        ),
        Some(&mint_pubkey),
    );
    bank_client
        .send_and_confirm_message(&[&mint_keypair, &vote_keypair, &identity_keypair], message)
        .expect("failed to create vote account");

    let authorized = Authorized::auto(&stake_pubkey);
    let bonus_delegation = 1_000_000_000;
    let stake_starting_delegation =
        2 * stake_minimum_delegation + bonus_delegation + stake_rent_exempt_reserve;
    let stake_starting_balance = stake_starting_delegation + stake_rent_exempt_reserve;

    // Create stake account and delegate to vote account
    let message = Message::new(
        &stake_instruction::create_account_and_delegate_stake(
            &mint_pubkey,
            &stake_pubkey,
            &vote_pubkey,
            &authorized,
            &Lockup::default(),
            stake_starting_balance,
        ),
        Some(&mint_pubkey),
    );
    bank_client
        .send_and_confirm_message(&[&mint_keypair, &stake_keypair], message)
        .expect("failed to create and delegate stake account");

    // Test that correct lamports are staked
    let account = bank.get_account(&stake_pubkey).expect("account not found");
    let stake_state = account.state().expect("couldn't unpack account data");
    if let StakeStateV2::Stake(_meta, stake, _stake_flags) = stake_state {
        assert_eq!(stake.delegation.stake, stake_starting_delegation,);
    } else {
        panic!("wrong account type found")
    }

    // Test that we cannot withdraw anything until deactivation
    let message = Message::new(
        &[stake_instruction::withdraw(
            &stake_pubkey,
            &stake_pubkey,
            &solana_pubkey::new_rand(),
            1,
            None,
        )],
        Some(&mint_pubkey),
    );
    assert!(bank_client
        .send_and_confirm_message(&[&mint_keypair, &stake_keypair], message)
        .is_err());

    // Test that lamports are still staked
    let account = bank.get_account(&stake_pubkey).expect("account not found");
    let stake_state = account.state().expect("couldn't unpack account data");
    if let StakeStateV2::Stake(_meta, stake, _stake_flags) = stake_state {
        assert_eq!(stake.delegation.stake, stake_starting_delegation,);
    } else {
        panic!("wrong account type found")
    }

    loop {
        if warmed_up(&bank, &stake_pubkey) {
            break;
        }
        // Cycle thru banks until we're fully warmed up
        bank = next_epoch_and_n_slots(bank, bank_forks.as_ref(), 0);
    }

    // Reward redemption
    // Submit enough votes to generate rewards
    let start_slot = bank.slot();
    bank = fill_epoch_with_votes(
        bank,
        bank_forks.as_ref(),
        &vote_keypair,
        &mint_keypair,
        start_slot,
    );

    // Test that votes and credits are there
    let account = bank.get_account(&vote_pubkey).expect("account not found");
    let vote_state: VoteStateV3 = StateMut::<VoteStateVersions>::state(&account)
        .expect("couldn't unpack account data")
        .convert_to_current();

    // 1 less vote, as the first vote should have cleared the lockout
    assert_eq!(vote_state.votes.len(), 31);
    // one vote per slot, might be more slots than 32 in the epoch
    assert!(vote_state.credits() >= 1);

    bank = fill_epoch_with_votes(
        bank,
        bank_forks.as_ref(),
        &vote_keypair,
        &mint_keypair,
        start_slot,
    );

    let pre_staked = get_staked(&bank, &stake_pubkey);
    let pre_balance = bank.get_balance(&stake_pubkey);

    // next epoch bank plus one additional slot should pay rewards
    bank = next_epoch_and_n_slots(bank, bank_forks.as_ref(), 1);

    // Test that balance increased, and that the balance got staked
    let staked = get_staked(&bank, &stake_pubkey);
    let balance = bank.get_balance(&stake_pubkey);
    assert!(staked > pre_staked);
    assert!(balance > pre_balance);

    // split the stake
    let split_stake_keypair = Keypair::new();
    let split_stake_pubkey = split_stake_keypair.pubkey();

    bank.transfer(
        stake_rent_exempt_reserve,
        &mint_keypair,
        &split_stake_pubkey,
    )
    .unwrap();
    let bank_client = BankClient::new_shared(bank.clone());

    // Test split
    let split_starting_delegation = stake_minimum_delegation + bonus_delegation;
    let message = Message::new(
        &stake_instruction::split(
            &stake_pubkey,
            &stake_pubkey,
            split_starting_delegation,
            &split_stake_pubkey,
        ),
        Some(&mint_pubkey),
    );
    assert!(bank_client
        .send_and_confirm_message(
            &[&mint_keypair, &stake_keypair, &split_stake_keypair],
            message
        )
        .is_ok());
    assert_eq!(
        get_staked(&bank, &split_stake_pubkey),
        split_starting_delegation,
    );
    let stake_remaining_balance = balance - split_starting_delegation;

    // Deactivate the split
    let message = Message::new(
        &[stake_instruction::deactivate_stake(
            &split_stake_pubkey,
            &stake_pubkey,
        )],
        Some(&mint_pubkey),
    );
    assert!(bank_client
        .send_and_confirm_message(&[&mint_keypair, &stake_keypair], message)
        .is_ok());
    assert_eq!(
        get_staked(&bank, &split_stake_pubkey),
        split_starting_delegation,
    );

    // Test that we cannot withdraw above what's staked
    let message = Message::new(
        &[stake_instruction::withdraw(
            &split_stake_pubkey,
            &stake_pubkey,
            &solana_pubkey::new_rand(),
            split_starting_delegation + 1,
            None,
        )],
        Some(&mint_pubkey),
    );
    assert!(bank_client
        .send_and_confirm_message(&[&mint_keypair, &stake_keypair], message)
        .is_err());

    let mut bank = next_epoch_and_n_slots(bank, bank_forks.as_ref(), 1);

    let bank_client = BankClient::new_shared(bank.clone());

    // assert we're still cooling down
    let split_staked = get_staked(&bank, &split_stake_pubkey);
    assert!(split_staked > 0);

    // withdrawal in cooldown
    let split_balance = bank.get_balance(&split_stake_pubkey);
    let message = Message::new(
        &[stake_instruction::withdraw(
            &split_stake_pubkey,
            &stake_pubkey,
            &solana_pubkey::new_rand(),
            split_balance,
            None,
        )],
        Some(&mint_pubkey),
    );
    assert!(bank_client
        .send_and_confirm_message(&[&mint_keypair, &stake_keypair], message)
        .is_err());

    // but we can withdraw unstaked
    let split_unstaked = split_balance - split_staked - stake_rent_exempt_reserve;
    assert!(split_unstaked > 0);
    let message = Message::new(
        &[stake_instruction::withdraw(
            &split_stake_pubkey,
            &stake_pubkey,
            &solana_pubkey::new_rand(),
            split_unstaked,
            None,
        )],
        Some(&mint_pubkey),
    );
    assert!(bank_client
        .send_and_confirm_message(&[&mint_keypair, &stake_keypair], message)
        .is_ok());

    // finish cooldown
    loop {
        if get_staked(&bank, &split_stake_pubkey) == 0 {
            break;
        }
        bank = next_epoch_and_n_slots(bank, bank_forks.as_ref(), 1);
    }
    let bank_client = BankClient::new_shared(bank.clone());

    // Test that we can withdraw everything else out of the split
    let split_remaining_balance = split_balance - split_unstaked;
    let message = Message::new(
        &[stake_instruction::withdraw(
            &split_stake_pubkey,
            &stake_pubkey,
            &solana_pubkey::new_rand(),
            split_remaining_balance,
            None,
        )],
        Some(&mint_pubkey),
    );
    assert!(bank_client
        .send_and_confirm_message(&[&mint_keypair, &stake_keypair], message)
        .is_ok());

    // verify all the math sums to zero
    assert_eq!(bank.get_balance(&split_stake_pubkey), 0);
    assert_eq!(bank.get_balance(&stake_pubkey), stake_remaining_balance);
}

#[test]
fn test_create_stake_account_from_seed() {
    let vote_keypair = Keypair::new();
    let vote_pubkey = vote_keypair.pubkey();
    let identity_keypair = Keypair::new();
    let identity_pubkey = identity_keypair.pubkey();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config_with_leader(100_000_000_000, &solana_pubkey::new_rand(), 1_000_000);
    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let mint_pubkey = mint_keypair.pubkey();
    let bank_client = BankClient::new_shared(bank.clone());

    let seed = "test-string";
    let stake_pubkey = Pubkey::create_with_seed(&mint_pubkey, seed, &stake::program::id()).unwrap();

    // Create Vote Account
    let message = Message::new(
        &vote_instruction::create_account_with_config(
            &mint_pubkey,
            &vote_pubkey,
            &VoteInit {
                node_pubkey: identity_pubkey,
                authorized_voter: vote_pubkey,
                authorized_withdrawer: vote_pubkey,
                commission: 50,
            },
            10,
            vote_instruction::CreateVoteAccountConfig {
                space: VoteStateVersions::vote_state_size_of(true) as u64,
                ..vote_instruction::CreateVoteAccountConfig::default()
            },
        ),
        Some(&mint_pubkey),
    );
    bank_client
        .send_and_confirm_message(&[&mint_keypair, &vote_keypair, &identity_keypair], message)
        .expect("failed to create vote account");

    let authorized = Authorized::auto(&mint_pubkey);
    let (balance, delegation) = {
        let rent = &bank.rent_collector().rent;
        let rent_exempt_reserve = rent.minimum_balance(StakeStateV2::size_of());
        let minimum_delegation = solana_stake_program::get_minimum_delegation(
            bank.feature_set
                .is_active(&agave_feature_set::stake_raise_minimum_delegation_to_1_sol::id()),
        );
        (rent_exempt_reserve + minimum_delegation, minimum_delegation)
    };

    // Create stake account and delegate to vote account
    let message = Message::new(
        &stake_instruction::create_account_with_seed_and_delegate_stake(
            &mint_pubkey,
            &stake_pubkey,
            &mint_pubkey,
            seed,
            &vote_pubkey,
            &authorized,
            &Lockup::default(),
            balance,
        ),
        Some(&mint_pubkey),
    );
    bank_client
        .send_and_confirm_message(&[&mint_keypair], message)
        .expect("failed to create and delegate stake account");

    // Test that correct lamports are staked
    let account = bank.get_account(&stake_pubkey).expect("account not found");
    let stake_state = account.state().expect("couldn't unpack account data");
    if let StakeStateV2::Stake(_meta, stake, _) = stake_state {
        assert_eq!(stake.delegation.stake, delegation);
    } else {
        panic!("wrong account type found")
    }
}

//! Staking for domains

use crate::pallet::{
    DomainStakingSummary, NextOperatorId, Nominators, OperatorIdOwner, OperatorPools,
    PendingDeposits, PendingOperatorDeregistrations, PendingOperatorSwitches, PendingWithdrawals,
};
use crate::{BalanceOf, Config, FreezeIdentifier, NominatorId};
use codec::{Decode, Encode};
use frame_support::traits::fungible::{Inspect, InspectFreeze, MutateFreeze};
use frame_support::traits::tokens::{Fortitude, Preservation};
use frame_support::{ensure, PalletError};
use scale_info::TypeInfo;
use sp_core::Get;
use sp_domains::{DomainId, EpochIndex, OperatorId, OperatorPublicKey};
use sp_runtime::traits::{CheckedAdd, CheckedSub, Zero};
use sp_runtime::{Perbill, Percent};
use sp_std::vec::Vec;

/// Type that represents an operator pool details.
#[derive(TypeInfo, Debug, Encode, Decode, Clone, PartialEq, Eq)]
pub struct OperatorPool<Balance, Share> {
    pub signing_key: OperatorPublicKey,
    pub current_domain_id: DomainId,
    pub next_domain_id: DomainId,
    pub minimum_nominator_stake: Balance,
    pub nomination_tax: Percent,
    /// Total active stake for the current pool.
    pub current_total_stake: Balance,
    /// Total rewards this operator received this current epoch.
    pub current_epoch_rewards: Balance,
    /// Total shares of the nominators and the operator in this pool.
    pub total_shares: Share,
    pub is_frozen: bool,
}

/// Type that represents a nominator's details under a specific operator pool
#[derive(TypeInfo, Debug, Encode, Decode, Clone, PartialEq, Eq)]
pub struct Nominator<Share> {
    pub shares: Share,
}

#[derive(TypeInfo, Debug, Encode, Decode, Clone, PartialEq, Eq)]
pub enum Withdraw<Balance> {
    All,
    Some(Balance),
}

#[derive(TypeInfo, Debug, Encode, Decode, Clone, PartialEq, Eq)]
pub struct StakingSummary<OperatorId, Balance> {
    /// Current epoch index for the domain.
    pub current_epoch_index: EpochIndex,
    /// Total active stake for the current epoch.
    pub current_total_stake: Balance,
    /// Current operators for this epoch
    pub current_operators: Vec<OperatorId>,
    /// Operators for the next epoch.
    pub next_operators: Vec<OperatorId>,
}

#[derive(TypeInfo, Debug, Encode, Decode, Clone, PartialEq, Eq)]
pub struct OperatorConfig<Balance> {
    pub signing_key: OperatorPublicKey,
    pub minimum_nominator_stake: Balance,
    pub nomination_tax: Percent,
}

#[derive(TypeInfo, Encode, Decode, PalletError, Debug, PartialEq)]
pub enum Error {
    MaximumOperatorId,
    DomainNotInitialized,
    InsufficientBalance,
    BalanceFreeze,
    MinimumOperatorStake,
    UnknownOperator,
    MinimumNominatorStake,
    BalanceOverflow,
    BalanceUnderflow,
    NotOperatorOwner,
    OperatorPoolFrozen,
    UnknownNominator,
    ExistingFullWithdraw,
}

pub(crate) fn do_register_operator<T: Config>(
    operator_owner: T::AccountId,
    domain_id: DomainId,
    amount: BalanceOf<T>,
    config: OperatorConfig<BalanceOf<T>>,
) -> Result<OperatorId, Error> {
    DomainStakingSummary::<T>::try_mutate(domain_id, |maybe_domain_stake_summary| {
        let operator_id = NextOperatorId::<T>::get();
        let next_operator_id = operator_id.checked_add(1).ok_or(Error::MaximumOperatorId)?;
        NextOperatorId::<T>::set(next_operator_id);

        OperatorIdOwner::<T>::insert(operator_id, operator_owner.clone());

        // reserve stake balance
        ensure!(
            amount >= T::MinOperatorStake::get(),
            Error::MinimumOperatorStake
        );

        freeze_account_balance_to_operator::<T>(&operator_owner, operator_id, amount)?;

        let domain_stake_summary = maybe_domain_stake_summary
            .as_mut()
            .ok_or(Error::DomainNotInitialized)?;

        let OperatorConfig {
            signing_key,
            minimum_nominator_stake,
            nomination_tax,
        } = config;

        let operator = OperatorPool {
            signing_key,
            current_domain_id: domain_id,
            next_domain_id: domain_id,
            minimum_nominator_stake,
            nomination_tax,
            current_total_stake: Zero::zero(),
            current_epoch_rewards: Zero::zero(),
            total_shares: Zero::zero(),
            is_frozen: false,
        };
        OperatorPools::<T>::insert(operator_id, operator);
        // update stake summary to include new operator for next epoch
        domain_stake_summary.next_operators.push(operator_id);
        // update pending transfers
        PendingDeposits::<T>::insert(operator_id, operator_owner, amount);

        Ok(operator_id)
    })
}

pub(crate) fn do_nominate_operator<T: Config>(
    operator_id: OperatorId,
    nominator_id: T::AccountId,
    amount: BalanceOf<T>,
) -> Result<(), Error> {
    let operator_pool = OperatorPools::<T>::get(operator_id).ok_or(Error::UnknownOperator)?;

    ensure!(!operator_pool.is_frozen, Error::OperatorPoolFrozen);

    let updated_total_deposit = match PendingDeposits::<T>::get(operator_id, nominator_id.clone()) {
        None => amount,
        Some(existing_deposit) => existing_deposit
            .checked_add(&amount)
            .ok_or(Error::BalanceOverflow)?,
    };

    ensure!(
        updated_total_deposit >= operator_pool.minimum_nominator_stake,
        Error::MinimumNominatorStake
    );

    freeze_account_balance_to_operator::<T>(&nominator_id, operator_id, amount)?;
    PendingDeposits::<T>::insert(operator_id, nominator_id, updated_total_deposit);

    Ok(())
}

fn freeze_account_balance_to_operator<T: Config>(
    who: &T::AccountId,
    operator_id: OperatorId,
    amount: BalanceOf<T>,
) -> Result<(), Error> {
    // ensure there is enough free balance to lock
    ensure!(
        T::Currency::reducible_balance(who, Preservation::Protect, Fortitude::Polite) >= amount,
        Error::InsufficientBalance
    );

    let freeze_id = T::FreezeIdentifier::staking_freeze_id(operator_id);
    // lock any previous locked balance + new deposit
    let current_locked_balance = T::Currency::balance_frozen(&freeze_id, who);
    let balance_to_be_locked = current_locked_balance
        .checked_add(&amount)
        .ok_or(Error::BalanceOverflow)?;

    T::Currency::set_freeze(&freeze_id, who, balance_to_be_locked)
        .map_err(|_| Error::BalanceFreeze)?;

    Ok(())
}

pub(crate) fn do_switch_operator_domain<T: Config>(
    operator_owner: T::AccountId,
    operator_id: OperatorId,
    new_domain_id: DomainId,
) -> Result<DomainId, Error> {
    ensure!(
        OperatorIdOwner::<T>::get(operator_id) == Some(operator_owner),
        Error::NotOperatorOwner
    );

    ensure!(
        DomainStakingSummary::<T>::contains_key(new_domain_id),
        Error::DomainNotInitialized
    );

    OperatorPools::<T>::try_mutate(operator_id, |maybe_operator_pool| {
        let operator_pool = maybe_operator_pool.as_mut().ok_or(Error::UnknownOperator)?;

        ensure!(!operator_pool.is_frozen, Error::OperatorPoolFrozen);
        operator_pool.next_domain_id = new_domain_id;

        // remove operator from next_operators from current domains.
        // operator is added to the next_operators of the new domain once the
        // current domain epoch is finished.
        DomainStakingSummary::<T>::try_mutate(
            operator_pool.current_domain_id,
            |maybe_domain_stake_summary| {
                let stake_summary = maybe_domain_stake_summary
                    .as_mut()
                    .ok_or(Error::DomainNotInitialized)?;
                stake_summary
                    .next_operators
                    .retain(|val| *val != operator_id);
                Ok(())
            },
        )?;

        PendingOperatorSwitches::<T>::append(operator_pool.current_domain_id, operator_id);

        Ok(operator_pool.current_domain_id)
    })
}

pub(crate) fn do_deregister_operator<T: Config>(
    operator_owner: T::AccountId,
    operator_id: OperatorId,
) -> Result<(), Error> {
    ensure!(
        OperatorIdOwner::<T>::get(operator_id) == Some(operator_owner),
        Error::NotOperatorOwner
    );

    OperatorPools::<T>::try_mutate(operator_id, |maybe_operator_pool| {
        let operator_pool = maybe_operator_pool.as_mut().ok_or(Error::UnknownOperator)?;

        ensure!(!operator_pool.is_frozen, Error::OperatorPoolFrozen);
        operator_pool.is_frozen = true;

        DomainStakingSummary::<T>::try_mutate(
            operator_pool.current_domain_id,
            |maybe_domain_stake_summary| {
                let stake_summary = maybe_domain_stake_summary
                    .as_mut()
                    .ok_or(Error::DomainNotInitialized)?;

                stake_summary
                    .next_operators
                    .retain(|val| *val != operator_id);
                Ok(())
            },
        )?;

        PendingOperatorDeregistrations::<T>::append(operator_id);

        Ok(())
    })
}

pub(crate) fn do_withdraw_stake<T: Config>(
    operator_id: OperatorId,
    nominator_id: NominatorId<T>,
    withdraw: Withdraw<BalanceOf<T>>,
) -> Result<(), Error> {
    OperatorPools::<T>::try_mutate(operator_id, |maybe_operator_pool| {
        let operator_pool = maybe_operator_pool.as_mut().ok_or(Error::UnknownOperator)?;
        ensure!(!operator_pool.is_frozen, Error::OperatorPoolFrozen);

        let nominator = Nominators::<T>::get(operator_id, nominator_id.clone())
            .ok_or(Error::UnknownNominator)?;

        let operator_owner =
            OperatorIdOwner::<T>::get(operator_id).ok_or(Error::UnknownOperator)?;

        let withdraw = match PendingWithdrawals::<T>::get(operator_id, nominator_id.clone()) {
            None => withdraw,
            Some(existing_withdraw) => match (existing_withdraw, withdraw) {
                (Withdraw::All, _) => {
                    // there is an existing full withdraw, error out
                    return Err(Error::ExistingFullWithdraw);
                }
                (_, Withdraw::All) => {
                    // there is exisiting withdrawal with specific amount,
                    // since the new intent is complete withdrawl, use this instead
                    Withdraw::All
                }
                (Withdraw::Some(previous_withdraw), Withdraw::Some(new_withdraw)) => {
                    // combine both withdrawls into single one
                    Withdraw::Some(
                        previous_withdraw
                            .checked_add(&new_withdraw)
                            .ok_or(Error::BalanceOverflow)?,
                    )
                }
            },
        };

        match withdraw {
            Withdraw::All => {
                // if nominator is the operator pool owner and trying to withdraw all, then error out
                if operator_owner == nominator_id {
                    return Err(Error::MinimumOperatorStake);
                }

                PendingWithdrawals::<T>::insert(operator_id, nominator_id, withdraw);
            }
            Withdraw::Some(withdraw_amount) => {
                let total_pool_stake = operator_pool
                    .current_total_stake
                    .checked_add(&operator_pool.current_epoch_rewards)
                    .ok_or(Error::BalanceOverflow)?;

                let nominator_share =
                    Perbill::from_rational(nominator.shares, operator_pool.total_shares);

                let nominator_staked_amount = nominator_share * total_pool_stake;

                let nominator_remaining_amount = nominator_staked_amount
                    .checked_sub(&withdraw_amount)
                    .ok_or(Error::BalanceUnderflow)?;

                if operator_owner == nominator_id {
                    // for operator pool owner, the remaining amount should not be less than MinimumOperatorStake,
                    if nominator_remaining_amount < T::MinOperatorStake::get() {
                        return Err(Error::MinimumOperatorStake);
                    }

                    PendingWithdrawals::<T>::insert(operator_id, nominator_id, withdraw);

                    // for just a nominator, if remaining amount falls below MinimumNominator stake, then withdraw all
                    // else withdraw the asked amount only
                } else if nominator_remaining_amount < operator_pool.minimum_nominator_stake {
                    PendingWithdrawals::<T>::insert(operator_id, nominator_id, Withdraw::All);
                } else {
                    PendingWithdrawals::<T>::insert(operator_id, nominator_id, withdraw);
                }
            }
        }

        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use crate::pallet::{
        DomainStakingSummary, NextOperatorId, Nominators, OperatorIdOwner, OperatorPools,
        PendingDeposits, PendingOperatorDeregistrations, PendingOperatorSwitches,
        PendingWithdrawals,
    };
    use crate::staking::{
        Error as StakingError, Nominator, OperatorConfig, OperatorPool, StakingSummary, Withdraw,
    };
    use crate::tests::{new_test_ext, RuntimeOrigin, Test};
    use crate::{BalanceOf, Error, NominatorId};
    use frame_support::traits::fungible::Mutate;
    use frame_support::{assert_err, assert_ok};
    use sp_core::{Pair, U256};
    use sp_domains::{DomainId, OperatorPair};
    use sp_runtime::traits::Zero;
    use std::vec;
    use subspace_runtime_primitives::SSC;

    type Balances = pallet_balances::Pallet<Test>;
    type Domains = crate::Pallet<Test>;

    #[test]
    fn register_operator() {
        let domain_id = DomainId::new(0);
        let operator_account = 1;
        let operator_free_balance = 1500 * SSC;
        let operator_stake = 1000 * SSC;
        let pair = OperatorPair::from_seed(&U256::from(0u32).into());

        let mut ext = new_test_ext();
        ext.execute_with(|| {
            Balances::set_balance(&operator_account, operator_free_balance);
            assert!(Balances::usable_balance(operator_account) == operator_free_balance);

            DomainStakingSummary::<Test>::insert(
                domain_id,
                StakingSummary {
                    current_epoch_index: 0,
                    current_total_stake: 0,
                    current_operators: vec![],
                    next_operators: vec![],
                },
            );

            let operator_config = OperatorConfig {
                signing_key: pair.public(),
                minimum_nominator_stake: 0,
                nomination_tax: Default::default(),
            };

            let res = Domains::register_operator(
                RuntimeOrigin::signed(operator_account),
                domain_id,
                operator_stake,
                operator_config.clone(),
            );
            assert_ok!(res);

            assert_eq!(NextOperatorId::<Test>::get(), 1);
            // operator_id should be 0 and be registered
            assert_eq!(OperatorIdOwner::<Test>::get(0).unwrap(), operator_account);
            assert_eq!(
                OperatorPools::<Test>::get(0).unwrap(),
                OperatorPool {
                    signing_key: pair.public(),
                    current_domain_id: domain_id,
                    next_domain_id: domain_id,
                    minimum_nominator_stake: 0,
                    nomination_tax: Default::default(),
                    current_total_stake: 0,
                    current_epoch_rewards: 0,
                    total_shares: 0,
                    is_frozen: false,
                }
            );
            let pending_deposit = PendingDeposits::<Test>::get(0, operator_account).unwrap();
            assert_eq!(pending_deposit, operator_stake);

            assert_eq!(
                Balances::usable_balance(operator_account),
                operator_free_balance - operator_stake
            );

            // cannot use the locked funds to register a new operator
            let res = Domains::register_operator(
                RuntimeOrigin::signed(operator_account),
                domain_id,
                operator_stake,
                operator_config,
            );
            assert_err!(
                res,
                Error::<Test>::Staking(crate::staking::Error::InsufficientBalance)
            )
        });
    }

    #[test]
    fn nominate_operator() {
        let domain_id = DomainId::new(0);
        let operator_account = 1;
        let operator_free_balance = 1500 * SSC;
        let operator_stake = 1000 * SSC;
        let pair = OperatorPair::from_seed(&U256::from(0u32).into());

        let nominator_account = 2;
        let nominator_free_balance = 150 * SSC;
        let nominator_stake = 100 * SSC;

        let mut ext = new_test_ext();
        ext.execute_with(|| {
            Balances::set_balance(&operator_account, operator_free_balance);
            Balances::set_balance(&nominator_account, nominator_free_balance);
            assert!(Balances::usable_balance(nominator_account) == nominator_free_balance);

            DomainStakingSummary::<Test>::insert(
                domain_id,
                StakingSummary {
                    current_epoch_index: 0,
                    current_total_stake: 0,
                    current_operators: vec![],
                    next_operators: vec![],
                },
            );

            let operator_config = OperatorConfig {
                signing_key: pair.public(),
                minimum_nominator_stake: 100 * SSC,
                nomination_tax: Default::default(),
            };

            let res = Domains::register_operator(
                RuntimeOrigin::signed(operator_account),
                domain_id,
                operator_stake,
                operator_config,
            );
            assert_ok!(res);

            let operator_id = 0;
            let res = Domains::nominate_operator(
                RuntimeOrigin::signed(nominator_account),
                operator_id,
                nominator_stake,
            );
            assert_ok!(res);

            let pending_deposit = PendingDeposits::<Test>::get(0, operator_account).unwrap();
            assert_eq!(pending_deposit, operator_stake);
            let pending_deposit = PendingDeposits::<Test>::get(0, nominator_account).unwrap();
            assert_eq!(pending_deposit, nominator_stake);

            assert_eq!(
                Balances::usable_balance(nominator_account),
                nominator_free_balance - nominator_stake
            );

            // another transfer with an existing transfer in place should lead to single
            let res = Domains::nominate_operator(
                RuntimeOrigin::signed(nominator_account),
                operator_id,
                40 * SSC,
            );
            assert_ok!(res);
            let pending_deposit = PendingDeposits::<Test>::get(0, nominator_account).unwrap();
            assert_eq!(pending_deposit, nominator_stake + 40 * SSC);
        });
    }

    #[test]
    fn switch_domain_operator() {
        let old_domain_id = DomainId::new(0);
        let new_domain_id = DomainId::new(1);
        let operator_account = 1;
        let operator_id = 1;
        let pair = OperatorPair::from_seed(&U256::from(0u32).into());

        let mut ext = new_test_ext();
        ext.execute_with(|| {
            DomainStakingSummary::<Test>::insert(
                old_domain_id,
                StakingSummary {
                    current_epoch_index: 0,
                    current_total_stake: 0,
                    current_operators: vec![operator_id],
                    next_operators: vec![operator_id],
                },
            );

            DomainStakingSummary::<Test>::insert(
                new_domain_id,
                StakingSummary {
                    current_epoch_index: 0,
                    current_total_stake: 0,
                    current_operators: vec![],
                    next_operators: vec![],
                },
            );

            OperatorIdOwner::<Test>::insert(operator_id, operator_account);
            OperatorPools::<Test>::insert(
                operator_id,
                OperatorPool {
                    signing_key: pair.public(),
                    current_domain_id: old_domain_id,
                    next_domain_id: old_domain_id,
                    minimum_nominator_stake: 100 * SSC,
                    nomination_tax: Default::default(),
                    current_total_stake: Zero::zero(),
                    current_epoch_rewards: Zero::zero(),
                    total_shares: Zero::zero(),
                    is_frozen: false,
                },
            );

            let res = Domains::switch_operator_domain(
                RuntimeOrigin::signed(operator_account),
                operator_id,
                new_domain_id,
            );
            assert_ok!(res);

            let old_domain_stake_summary =
                DomainStakingSummary::<Test>::get(old_domain_id).unwrap();
            assert!(!old_domain_stake_summary
                .next_operators
                .contains(&operator_id));

            let new_domain_stake_summary =
                DomainStakingSummary::<Test>::get(new_domain_id).unwrap();
            assert!(!new_domain_stake_summary
                .next_operators
                .contains(&operator_id));

            let operator_pool = OperatorPools::<Test>::get(operator_id).unwrap();
            assert_eq!(operator_pool.current_domain_id, old_domain_id);
            assert_eq!(operator_pool.next_domain_id, new_domain_id);
            assert_eq!(
                PendingOperatorSwitches::<Test>::get(old_domain_id).unwrap(),
                vec![operator_id]
            )
        });
    }

    #[test]
    fn operator_deregistration() {
        let domain_id = DomainId::new(0);
        let operator_account = 1;
        let operator_id = 1;
        let pair = OperatorPair::from_seed(&U256::from(0u32).into());

        let mut ext = new_test_ext();
        ext.execute_with(|| {
            DomainStakingSummary::<Test>::insert(
                domain_id,
                StakingSummary {
                    current_epoch_index: 0,
                    current_total_stake: 0,
                    current_operators: vec![operator_id],
                    next_operators: vec![operator_id],
                },
            );

            OperatorIdOwner::<Test>::insert(operator_id, operator_account);
            OperatorPools::<Test>::insert(
                operator_id,
                OperatorPool {
                    signing_key: pair.public(),
                    current_domain_id: domain_id,
                    next_domain_id: domain_id,
                    minimum_nominator_stake: 100 * SSC,
                    nomination_tax: Default::default(),
                    current_total_stake: Zero::zero(),
                    current_epoch_rewards: Zero::zero(),
                    total_shares: Zero::zero(),
                    is_frozen: false,
                },
            );

            let res =
                Domains::deregister_operator(RuntimeOrigin::signed(operator_account), operator_id);
            assert_ok!(res);

            let domain_stake_summary = DomainStakingSummary::<Test>::get(domain_id).unwrap();
            assert!(!domain_stake_summary.next_operators.contains(&operator_id));

            let operator_pool = OperatorPools::<Test>::get(operator_id).unwrap();
            assert!(operator_pool.is_frozen);

            assert!(PendingOperatorDeregistrations::<Test>::get()
                .unwrap()
                .contains(&operator_id));

            // domain switch will not work since the operator pool is frozen
            let new_domain_id = DomainId::new(1);
            DomainStakingSummary::<Test>::insert(
                new_domain_id,
                StakingSummary {
                    current_epoch_index: 0,
                    current_total_stake: 0,
                    current_operators: vec![],
                    next_operators: vec![],
                },
            );
            let res = Domains::switch_operator_domain(
                RuntimeOrigin::signed(operator_account),
                operator_id,
                new_domain_id,
            );
            assert_err!(
                res,
                Error::<Test>::Staking(crate::staking::Error::OperatorPoolFrozen)
            );

            // nominations will not work since the pool is frozen
            let nominator_account = 100;
            let nominator_stake = 100 * SSC;
            let res = Domains::nominate_operator(
                RuntimeOrigin::signed(nominator_account),
                operator_id,
                nominator_stake,
            );
            assert_err!(
                res,
                Error::<Test>::Staking(crate::staking::Error::OperatorPoolFrozen)
            );
        });
    }

    type WithdrawWithResult = Vec<(Withdraw<BalanceOf<Test>>, Result<(), StakingError>)>;

    struct WithdrawParams {
        minimum_nominator_stake: BalanceOf<Test>,
        total_stake: BalanceOf<Test>,
        nominators: Vec<(NominatorId<Test>, BalanceOf<Test>)>,
        operator_reward: BalanceOf<Test>,
        nominator_id: NominatorId<Test>,
        withdraws: WithdrawWithResult,
        expected_withdraw: Option<Withdraw<BalanceOf<Test>>>,
    }

    fn withdraw_stake(params: WithdrawParams) {
        let WithdrawParams {
            minimum_nominator_stake,
            total_stake,
            nominators,
            operator_reward,
            nominator_id,
            withdraws,
            expected_withdraw,
        } = params;
        let domain_id = DomainId::new(0);
        let operator_account = 0;
        let operator_id = 0;
        let pair = OperatorPair::from_seed(&U256::from(0u32).into());

        let mut ext = new_test_ext();
        ext.execute_with(|| {
            DomainStakingSummary::<Test>::insert(
                domain_id,
                StakingSummary {
                    current_epoch_index: 0,
                    current_total_stake: total_stake,
                    current_operators: vec![operator_id],
                    next_operators: vec![operator_id],
                },
            );

            OperatorIdOwner::<Test>::insert(operator_id, operator_account);

            let mut total_shares = Zero::zero();
            for (nominator_id, shares) in nominators {
                Nominators::<Test>::insert(operator_id, nominator_id, Nominator { shares });
                total_shares += shares
            }

            OperatorPools::<Test>::insert(
                operator_id,
                OperatorPool {
                    signing_key: pair.public(),
                    current_domain_id: domain_id,
                    next_domain_id: domain_id,
                    minimum_nominator_stake,
                    nomination_tax: Default::default(),
                    current_total_stake: total_stake,
                    current_epoch_rewards: operator_reward,
                    total_shares,
                    is_frozen: false,
                },
            );

            for (withdraw, expected_result) in withdraws {
                let res = Domains::withdraw_stake(
                    RuntimeOrigin::signed(nominator_id),
                    operator_id,
                    withdraw,
                );
                assert_eq!(
                    res,
                    expected_result.map_err(|err| Error::<Test>::Staking(err).into())
                );
            }

            assert_eq!(
                PendingWithdrawals::<Test>::get(operator_id, nominator_id),
                expected_withdraw
            )
        });
    }

    #[test]
    fn withdraw_stake_operator_all() {
        withdraw_stake(WithdrawParams {
            minimum_nominator_stake: 10 * SSC,
            total_stake: 210 * SSC,
            nominators: vec![(0, 150 * SSC), (1, 50 * SSC), (2, 10 * SSC)],
            operator_reward: 20 * SSC,
            nominator_id: 0,
            withdraws: vec![(Withdraw::All, Err(StakingError::MinimumOperatorStake))],
            expected_withdraw: None,
        })
    }

    #[test]
    fn withdraw_stake_operator_below_minimum() {
        withdraw_stake(WithdrawParams {
            minimum_nominator_stake: 10 * SSC,
            total_stake: 210 * SSC,
            nominators: vec![(0, 150 * SSC), (1, 50 * SSC), (2, 10 * SSC)],
            operator_reward: 20 * SSC,
            nominator_id: 0,
            withdraws: vec![(
                Withdraw::Some(65 * SSC),
                Err(StakingError::MinimumOperatorStake),
            )],
            expected_withdraw: None,
        })
    }

    #[test]
    fn withdraw_stake_operator_below_minimum_no_rewards() {
        withdraw_stake(WithdrawParams {
            minimum_nominator_stake: 10 * SSC,
            total_stake: 210 * SSC,
            nominators: vec![(0, 150 * SSC), (1, 50 * SSC), (2, 10 * SSC)],
            operator_reward: Zero::zero(),
            nominator_id: 0,
            withdraws: vec![(
                Withdraw::Some(51 * SSC),
                Err(StakingError::MinimumOperatorStake),
            )],
            expected_withdraw: None,
        })
    }

    #[test]
    fn withdraw_stake_operator_above_minimum() {
        withdraw_stake(WithdrawParams {
            minimum_nominator_stake: 10 * SSC,
            total_stake: 210 * SSC,
            nominators: vec![(0, 150 * SSC), (1, 50 * SSC), (2, 10 * SSC)],
            operator_reward: 20 * SSC,
            nominator_id: 0,
            withdraws: vec![(Withdraw::Some(64 * SSC), Ok(()))],
            expected_withdraw: Some(Withdraw::Some(64 * SSC)),
        })
    }

    #[test]
    fn withdraw_stake_operator_above_minimum_multiple_withdraws_error() {
        withdraw_stake(WithdrawParams {
            minimum_nominator_stake: 10 * SSC,
            total_stake: 210 * SSC,
            nominators: vec![(0, 150 * SSC), (1, 50 * SSC), (2, 10 * SSC)],
            operator_reward: 20 * SSC,
            nominator_id: 0,
            withdraws: vec![
                (Withdraw::Some(60 * SSC), Ok(())),
                (
                    Withdraw::Some(5 * SSC),
                    Err(StakingError::MinimumOperatorStake),
                ),
            ],
            expected_withdraw: Some(Withdraw::Some(60 * SSC)),
        })
    }

    #[test]
    fn withdraw_stake_operator_above_minimum_multiple_withdraws() {
        withdraw_stake(WithdrawParams {
            minimum_nominator_stake: 10 * SSC,
            total_stake: 210 * SSC,
            nominators: vec![(0, 150 * SSC), (1, 50 * SSC), (2, 10 * SSC)],
            operator_reward: 20 * SSC,
            nominator_id: 0,
            withdraws: vec![
                (Withdraw::Some(60 * SSC), Ok(())),
                (Withdraw::Some(4 * SSC), Ok(())),
            ],
            expected_withdraw: Some(Withdraw::Some(64 * SSC)),
        })
    }

    #[test]
    fn withdraw_stake_operator_above_minimum_no_rewards() {
        withdraw_stake(WithdrawParams {
            minimum_nominator_stake: 10 * SSC,
            total_stake: 210 * SSC,
            nominators: vec![(0, 150 * SSC), (1, 50 * SSC), (2, 10 * SSC)],
            operator_reward: Zero::zero(),
            nominator_id: 0,
            withdraws: vec![(Withdraw::Some(49 * SSC), Ok(()))],
            expected_withdraw: Some(Withdraw::Some(49 * SSC)),
        })
    }

    #[test]
    fn withdraw_stake_nominator_below_minimum() {
        withdraw_stake(WithdrawParams {
            minimum_nominator_stake: 10 * SSC,
            total_stake: 210 * SSC,
            nominators: vec![(0, 150 * SSC), (1, 50 * SSC), (2, 10 * SSC)],
            operator_reward: 20 * SSC,
            nominator_id: 1,
            withdraws: vec![(Withdraw::Some(45 * SSC), Ok(()))],
            expected_withdraw: Some(Withdraw::All),
        })
    }

    #[test]
    fn withdraw_stake_nominator_below_minimum_no_reward() {
        withdraw_stake(WithdrawParams {
            minimum_nominator_stake: 10 * SSC,
            total_stake: 210 * SSC,
            nominators: vec![(0, 150 * SSC), (1, 50 * SSC), (2, 10 * SSC)],
            operator_reward: Zero::zero(),
            nominator_id: 1,
            withdraws: vec![(Withdraw::Some(45 * SSC), Ok(()))],
            expected_withdraw: Some(Withdraw::All),
        })
    }

    #[test]
    fn withdraw_stake_nominator_above_minimum() {
        withdraw_stake(WithdrawParams {
            minimum_nominator_stake: 10 * SSC,
            total_stake: 210 * SSC,
            nominators: vec![(0, 150 * SSC), (1, 50 * SSC), (2, 10 * SSC)],
            operator_reward: 20 * SSC,
            nominator_id: 1,
            withdraws: vec![(Withdraw::Some(44 * SSC), Ok(()))],
            expected_withdraw: Some(Withdraw::Some(44 * SSC)),
        })
    }

    #[test]
    fn withdraw_stake_nominator_above_minimum_multiple_withdraw_all() {
        withdraw_stake(WithdrawParams {
            minimum_nominator_stake: 10 * SSC,
            total_stake: 210 * SSC,
            nominators: vec![(0, 150 * SSC), (1, 50 * SSC), (2, 10 * SSC)],
            operator_reward: 20 * SSC,
            nominator_id: 1,
            withdraws: vec![
                (Withdraw::Some(40 * SSC), Ok(())),
                (Withdraw::Some(5 * SSC), Ok(())),
            ],
            expected_withdraw: Some(Withdraw::All),
        })
    }

    #[test]
    fn withdraw_stake_nominator_withdraw_all() {
        withdraw_stake(WithdrawParams {
            minimum_nominator_stake: 10 * SSC,
            total_stake: 210 * SSC,
            nominators: vec![(0, 150 * SSC), (1, 50 * SSC), (2, 10 * SSC)],
            operator_reward: 20 * SSC,
            nominator_id: 1,
            withdraws: vec![(Withdraw::All, Ok(()))],
            expected_withdraw: Some(Withdraw::All),
        })
    }

    #[test]
    fn withdraw_stake_nominator_withdraw_all_multiple_withdraws_error() {
        withdraw_stake(WithdrawParams {
            minimum_nominator_stake: 10 * SSC,
            total_stake: 210 * SSC,
            nominators: vec![(0, 150 * SSC), (1, 50 * SSC), (2, 10 * SSC)],
            operator_reward: 20 * SSC,
            nominator_id: 1,
            withdraws: vec![
                (Withdraw::All, Ok(())),
                (
                    Withdraw::Some(10 * SSC),
                    Err(StakingError::ExistingFullWithdraw),
                ),
            ],
            expected_withdraw: Some(Withdraw::All),
        })
    }

    #[test]
    fn withdraw_stake_nominator_above_minimum_no_rewards() {
        withdraw_stake(WithdrawParams {
            minimum_nominator_stake: 10 * SSC,
            total_stake: 210 * SSC,
            nominators: vec![(0, 150 * SSC), (1, 50 * SSC), (2, 10 * SSC)],
            operator_reward: Zero::zero(),
            nominator_id: 1,
            withdraws: vec![(Withdraw::Some(39 * SSC), Ok(()))],
            expected_withdraw: Some(Withdraw::Some(39 * SSC)),
        })
    }
}

// Copyright 2017-2019 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.


#![recursion_limit = "128"]
#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(all(feature = "bench", test), feature(test))]

#[cfg(all(feature = "bench", test))]
extern crate test;


#[cfg(feature = "std")]
use runtime_io::with_storage;
use rstd::{prelude::*, result, collections::btree_map::BTreeMap};
use parity_codec::{HasCompact, Encode, Decode};
use srml_support::{
    StorageValue, StorageMap, EnumerableStorageMap, decl_module, decl_event,
    decl_storage, ensure, traits::{
        Currency, OnFreeBalanceZero, OnDilution, LockIdentifier, LockableCurrency,
        WithdrawReasons, OnUnbalanced, Imbalance, Get,
    },
};
use session::{OnSessionEnding, SessionIndex};
use primitives::Perbill;
use primitives::traits::{ SimpleArithmetic,
    Convert, Zero, One, StaticLookup, CheckedSub, CheckedShl, Saturating, Bounded, SaturatedConversion,
};
#[cfg(feature = "std")]
use primitives::{Serialize, Deserialize};
use system::ensure_signed;


//use phragmen::{ACCURACY, elect, equalize, ExtendedBalance};


mod utils;

#[cfg(any(feature = "bench", test))]
mod mock;
//
#[cfg(test)]
mod tests;

//mod phragmen;

//#[cfg(all(feature = "bench", test))]
//mod benches;

const RECENT_OFFLINE_COUNT: usize = 32;
const DEFAULT_MINIMUM_VALIDATOR_COUNT: u32 = 4;
const MAX_NOMINATIONS: usize = 16;
const MAX_UNSTAKE_THRESHOLD: u32 = 10;
const MAX_UNLOCKING_CHUNKS: usize = 32;
const STAKING_ID: LockIdentifier = *b"staking ";

/// Counter for the number of eras that have passed.
pub type EraIndex = u32;
// customed: counter for number of eras per epoch.
pub type ErasNums = u32;

pub type PowerBalance = u128;

#[cfg_attr(feature = "std", derive(Debug, Serialize, Deserialize))]
pub enum StakerStatus<AccountId> {
    /// Chilling.
    Idle,
    /// Declared desire in validating or already participating in it.
    Validator,
    /// Nominating for a group of other stakers.
    Nominator(Vec<AccountId>),
}

#[derive(PartialEq, Eq, Clone, Encode, Decode)]
#[cfg_attr(feature = "std", derive(Debug))]
pub struct ValidatorPrefs<RingBalance: HasCompact> {
    /// Validator should ensure this many more slashes than is necessary before being unstaked.
    #[codec(compact)]
    pub unstake_threshold: u32,
    /// percent of Reward that validator takes up-front; only the rest is split between themselves and
    /// nominators.
    #[codec(compact)]
    pub validator_payment_ratio: RingBalance,
}

impl<R: Default + HasCompact + Copy> Default for ValidatorPrefs<R> {
    fn default() -> Self {
        ValidatorPrefs {
            unstake_threshold: 3,
            validator_payment_ratio: Default::default(),
}
}
}

pub trait Distinguish<RingBalance, KtonBalance> {
    fn is_ring(&self) -> (bool, Option<RingBalance>);
    fn is_kton(&self) -> (bool, Option<KtonBalance>);
}

#[derive(PartialEq, Eq, Clone, Encode, Decode)]
#[cfg_attr(feature = "std", derive(Debug))]
pub enum StakingBalance<RingBalance, KtonBalance> {
    Ring(RingBalance),
    Kton(KtonBalance),
}

impl<
    RingBalance: HasCompact + Copy + Clone,
    KtonBalance: HasCompact + Copy + Clone,
> Distinguish<RingBalance, KtonBalance> for StakingBalance<RingBalance, KtonBalance> {

    fn is_ring(&self) -> (bool, Option<RingBalance>) {
        let res = match self {
            StakingBalance::Ring(r) => (true, Some(*r)),
            StakingBalance::Kton(_) => (false, None),
        };
        res
    }


    fn is_kton(&self) -> (bool, Option<KtonBalance>) {
        let res = match self {
            StakingBalance::Ring(_) => (true, None),
            StakingBalance::Kton(k) => (false, Some(*k)),
        };
        res
    }
}

impl<
    RingBalance: Default,
    KtonBalance: Default> Default for StakingBalance<RingBalance, KtonBalance> {
    fn default() -> Self {
        StakingBalance::Ring(Default::default())
    }
}

/// A destination account for payment.
#[derive(PartialEq, Eq, Copy, Clone, Encode, Decode)]
#[cfg_attr(feature = "std", derive(Debug))]
pub enum RewardDestination {
    /// Pay into the stash account, increasing the amount at stake accordingly.
    /// for now, we dont use this.
//    DeprecatedStaked,
    /// Pay into the stash account, not increasing the amount at stake.
    Stash,
    /// Pay into the controller account.
    Controller,
}

impl Default for RewardDestination {
    fn default() -> Self {
        RewardDestination::Stash
    }
}


#[derive(PartialEq, Eq, Clone, Encode, Decode)]
#[cfg_attr(feature = "std", derive(Debug))]
pub struct UnlockChunk<StakingBalance, Power> {
    /// Amount of funds to be unlocked.
    value: StakingBalance,
    /// Era number at which point it'll be unlocked.
    #[codec(compact)]
    era: EraIndex,
    dt_power: Power,
}

#[derive(PartialEq, Eq, Clone, Encode, Decode)]
#[cfg_attr(feature = "std", derive(Debug))]
pub struct RegularItem<RingBalance: HasCompact, Moment> {
    #[codec(compact)]
    value: RingBalance,
    #[codec(compact)]
    expire_time: Moment,
}

#[derive(PartialEq, Eq, Clone, Encode, Decode, Default)]
#[cfg_attr(feature = "std", derive(Debug))]
pub struct StakingLedgers<AccountId, RingBalance: HasCompact, KtonBalance: HasCompact, StakingBalance, Power, Moment> {
    pub stash: AccountId,
    #[codec(compact)]
    pub total_power: Power,
    #[codec(compact)]
    pub active_power: Power,
    // normal pattern: for ring
    /// total_ring = nomarl_ring + regular_ring
    #[codec(compact)]
    pub normal_ring: RingBalance,
    #[codec(compact)]
    pub regular_ring: RingBalance,
    #[codec(compact)]
    pub active_ring: RingBalance,
    /// total_kton = normal_kton
    #[codec(compact)]
    pub normal_kton: KtonBalance,
    #[codec(compact)]
    pub active_kton: KtonBalance,
    // regular pattern: for kton
    pub regular_items: Vec<RegularItem<RingBalance, Moment>>,
    pub unlocking: Vec<UnlockChunk<StakingBalance, Power>>,
}

impl<
    AccountId,
    RingBalance: HasCompact + Copy + Saturating,
    KtonBalance: HasCompact + Copy + Saturating,
    StakingBalance: Distinguish<RingBalance, KtonBalance>,
    Power: Copy + Saturating,
    Moment
> StakingLedgers<AccountId, RingBalance, KtonBalance, StakingBalance, Power, Moment> {
    //
    fn consolidate_unlocked(self, current_era: EraIndex) -> (Self, u32) {
        // active_power and regular_ring already changed when `unbond`
        // here reduce total_power and normal_ring or normal_kton
        let mut total_power = self.total_power;
        let mut normal_ring = self.normal_ring;
        let mut normal_kton = self.normal_kton;

        let mut unlock_ring = 0u32;
        let mut unlock_kton = 0u32;
        let unlocking = self.unlocking.into_iter().filter(|chunk| if chunk.era > current_era {
            true
        } else {
            // for ring
            if chunk.value.is_ring().0 {
                total_power = total_power.saturating_sub(chunk.dt_power);
                normal_ring = normal_ring.saturating_sub(chunk.value.is_ring().1.unwrap());
                unlock_ring = 1;
                false
            } else if chunk.value.is_kton().0 {
                total_power = total_power.saturating_sub(chunk.dt_power);
                normal_kton = normal_kton.saturating_sub(chunk.value.is_kton().1.unwrap());
                unlock_kton = 2;
                false
            } else {
                // no ring or kton
                // discard it
                false
            }
        }).collect();

        (Self { total_power, normal_ring, normal_kton, unlocking, ..self }, unlock_ring + unlock_kton)
    }
}

/// The amount of exposure (to slashing) than an individual nominator has.
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Encode, Decode)]
#[cfg_attr(feature = "std", derive(Debug))]
pub struct IndividualExposure<AccountId, Power> {
    /// The stash account of the nominator in question.
    who: AccountId,
    /// Amount of funds exposed.
    value: Power,
}

/// A snapshot of the stake backing a single validator in the system.
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Encode, Decode, Default)]
#[cfg_attr(feature = "std", derive(Debug))]
pub struct Exposure<AccountId, Power> {
    /// The total balance backing this validator.
    pub total: Power,
    /// The validator's own stash that is exposed.
    pub own: Power,
    /// The portions of nominators stashes that are exposed.
    pub others: Vec<IndividualExposure<AccountId, Power>>,
}


type RingBalanceOf<T> = <<T as Trait>::Ring as Currency<<T as system::Trait>::AccountId>>::Balance;
type KtonBalanceOf<T> = <<T as Trait>::Kton as Currency<<T as system::Trait>::AccountId>>::Balance;

// for ring
type PositiveImbalanceOf<T> =
<<T as Trait>::Ring as Currency<<T as system::Trait>::AccountId>>::PositiveImbalance;
type NegativeImbalanceOf<T> =
<<T as Trait>::Ring as Currency<<T as system::Trait>::AccountId>>::NegativeImbalance;

type ExpoMap<T> = BTreeMap<
    <T as system::Trait>::AccountId,
    Exposure<<T as system::Trait>::AccountId, PowerBalance>
>;


pub trait Trait: timestamp::Trait + session::Trait {
    type Ring: LockableCurrency<Self::AccountId, Moment=Self::BlockNumber>;
    type Kton: LockableCurrency<Self::AccountId, Moment=Self::BlockNumber>;

//    type Power: SimpleArithmetic + Saturating + Convert<RingBalanceOf<Self>, PowerBalance> + Convert<KtonBalanceOf<Self>, PowerBalance>;
    // basic token
    type CurrencyToVote: Convert<KtonBalanceOf<Self>, u64> + Convert<u128, KtonBalanceOf<Self>>;

    /// The overarching event type.
    type Event: From<Event<Self>> + Into<<Self as system::Trait>::Event>;

    /// Handler for the unbalanced reduction when slashing a staker.
    type Slash: OnUnbalanced<NegativeImbalanceOf<Self>>;

    /// Handler for the unbalanced increment when rewarding a staker.
    type Reward: OnUnbalanced<PositiveImbalanceOf<Self>>;

    /// Number of sessions per era.
    type SessionsPerEra: Get<SessionIndex>;

    /// Number of eras that staked funds must remain bonded for.
    type BondingDuration: Get<EraIndex>;

    // custom
    type Cap: Get<<Self::Ring as Currency<Self::AccountId>>::Balance>;
    type ErasPerEpoch: Get<ErasNums>;
}

decl_storage! {
    trait Store for Module<T: Trait> as Staking {

		pub ValidatorCount get(validator_count) config(): u32;

		pub MinimumValidatorCount get(minimum_validator_count) config():
			u32 = DEFAULT_MINIMUM_VALIDATOR_COUNT;

		pub SessionReward get(session_reward) config(): Perbill = Perbill::from_parts(60);

		pub OfflineSlash get(offline_slash) config(): Perbill = Perbill::from_millionths(1000);

		pub OfflineSlashGrace get(offline_slash_grace) config(): u32;

		pub Invulnerables get(invulnerables) config(): Vec<T::AccountId>;

        pub Bonded get(bonded): map T::AccountId => Option<T::AccountId>;

        pub Ledger get(ledger): map T::AccountId => Option<StakingLedgers<
            T::AccountId, RingBalanceOf<T>, KtonBalanceOf<T>, StakingBalance<RingBalanceOf<T>, KtonBalanceOf<T>>,
            PowerBalance, T::Moment>>;

		pub Payee get(payee): map T::AccountId => RewardDestination;

		pub Validators get(validators): linked_map T::AccountId => ValidatorPrefs<RingBalanceOf<T>>;

		pub Nominators get(nominators): linked_map T::AccountId => Vec<T::AccountId>;

		pub Stakers get(stakers): map T::AccountId => Exposure<T::AccountId, PowerBalance>;

		pub CurrentElected get(current_elected): Vec<T::AccountId>;

		pub CurrentEra get(current_era) config(): EraIndex;

		pub CurrentSessionReward get(current_session_reward) config(): RingBalanceOf<T>;

		pub CurrentEraReward get(current_era_reward): RingBalanceOf<T>;

		pub SlotStake get(slot_stake) build(|config: &GenesisConfig<T>| {
			config.stakers.iter().map(|&(_, _, value, _)| value).min().unwrap_or_default()
		}): RingBalanceOf<T>;

		pub SlashCount get(slash_count): map T::AccountId => u32;

		pub RecentlyOffline get(recently_offline): Vec<(T::AccountId, T::BlockNumber, u32)>;

		pub ForceNewEra get(forcing_new_era): bool;

		pub EpochIndex get(epoch_index): T::BlockNumber = 0.into();

		/// The accumulated reward for the current era. Reset to zero at the beginning of the era
		/// and increased for every successfully finished session.
		pub CurrentEraTotalReward get(current_era_total_reward) config(): RingBalanceOf<T>;

		pub ShouldOffline get(should_offline): Vec<T::AccountId>;

		pub NodeName get(node_name): map T::AccountId => Vec<u8>;
    }
    add_extra_genesis {
		config(stakers):
			Vec<(T::AccountId, T::AccountId, RingBalanceOf<T>, StakerStatus<T::AccountId>)>;
		build(|
			storage: &mut primitives::StorageOverlay,
			_: &mut primitives::ChildrenStorageOverlay,
			config: &GenesisConfig<T>
		| {
			with_storage(storage, || {
				for &(ref stash, ref controller, balance, ref status) in &config.stakers {
					assert!(T::Ring::free_balance(&stash) >= balance);
					let _ = <Module<T>>::bond(
						T::Origin::from(Some(stash.clone()).into()),
						T::Lookup::unlookup(controller.clone()),
						StakingBalance::Ring(balance),
						RewardDestination::Stash,
						12
					);
					let _ = match status {
						StakerStatus::Validator => {
							<Module<T>>::validate(
								T::Origin::from(Some(controller.clone()).into()),
								[0;8].to_vec(),
								0,
								3
							)
						},
						StakerStatus::Nominator(votes) => {
							<Module<T>>::nominate(
								T::Origin::from(Some(controller.clone()).into()),
								votes.iter().map(|l| {T::Lookup::unlookup(l.clone())}).collect()
							)
						}, _ => Ok(())
					};
				}

//				if let (_, Some(validators)) = <Module<T>>::select_validators() {
//					<session::Validators<T>>::put(&validators);
//				}
			});
		});
	}
}

decl_event!(
    pub enum Event<T> where Balance = RingBalanceOf<T>, <T as system::Trait>::AccountId {
        /// All validators have been rewarded by the given balance.
		Reward(Balance),
		/// One validator (and its nominators) has been given an offline-warning (it is still
		/// within its grace). The accrued number of slashes is recorded, too.
		OfflineWarning(AccountId, u32),
		/// One validator (and its nominators) has been slashed by the given amount.
		OfflineSlash(AccountId, Balance),
    }
);

decl_module! {
    pub struct Module<T: Trait> for enum Call where origin: T::Origin {
        /// Number of sessions per era.
        const SessionsPerEra: SessionIndex = T::SessionsPerEra::get();

		/// Number of eras that staked funds must remain bonded for.
		const BondingDuration: EraIndex = T::BondingDuration::get();

		fn deposit_event<T>() = default;

        fn bond(origin,
            controller: <T::Lookup as StaticLookup>::Source,
            value: StakingBalance<RingBalanceOf<T>, KtonBalanceOf<T>>,
            payee: RewardDestination,
            promise_month: u32
        ) {
            let stash = ensure_signed(origin)?;
            ensure!( promise_month <= 36, "months at most is 36.");

			if <Bonded<T>>::exists(&stash) {
				return Err("stash already bonded")
			}

			let controller = T::Lookup::lookup(controller)?;

			if <Ledger<T>>::exists(&controller) {
				return Err("controller already paired")
			}

			<Bonded<T>>::insert(&stash, controller.clone());
			<Payee<T>>::insert(&stash, payee);

            let mut ledger = StakingLedgers {stash: stash.clone(), ..Default::default()};
			match value {
			    StakingBalance::Ring(r) => {
			        let stash_balance = T::Ring::free_balance(&stash);
			        let value = r.min(stash_balance);
			        Self::bond_helper_in_ring(stash.clone(), controller.clone(), value, promise_month, ledger)},
			    StakingBalance::Kton(k) => {
			        let stash_balance = T::Kton::free_balance(&stash);
			        let value = k.min(stash_balance);
			        Self::bond_helper_in_kton(stash.clone(), controller.clone(), value, ledger);
			    },
			}
        }

        fn bond_extra(origin,
            value: StakingBalance<RingBalanceOf<T>, KtonBalanceOf<T>>,
            promise_month: u32
        ) {
            let stash = ensure_signed(origin)?;
            ensure!( promise_month <= 36, "months at most is 36.");
			let controller = Self::bonded(&stash).ok_or("not a stash")?;
			let mut ledger = Self::ledger(&controller).ok_or("not a controller")?;
            let stash_balance = T::Ring::free_balance(&stash);
            match value {
                 StakingBalance::Ring(r) => {
                    let stash_balance = T::Ring::free_balance(&stash);
                    if let Some(extra) = stash_balance.checked_sub(&(ledger.normal_ring + ledger.regular_ring)) {
                        let extra = extra.min(r);
                        Self::bond_helper_in_ring(stash.clone(), controller.clone(), extra, promise_month, ledger);
                    }
                },

                StakingBalance::Kton(k) => {
                    let stash_balance = T::Kton::free_balance(&stash);
                    if let Some(extra) = stash_balance.checked_sub(&(ledger.normal_kton)) {
                        let extra = extra.min(k);
                        Self::bond_helper_in_kton(stash.clone(), controller.clone(), extra, ledger);
                    }
                },
            }
        }


        /// for normal_ring or normal_kton, follow the original substrate pattern
        /// for regular_ring, transform it into normal_ring first
        /// modify regular_items and regular_ring amount
        fn unbond(origin, value: StakingBalance<RingBalanceOf<T>, KtonBalanceOf<T>>, is_regular: bool) {
            let controller = ensure_signed(origin)?;

            let mut ledger = Self::ledger(&controller).ok_or("not a controller")?;
//            let regular_items = ledger.regular_items;
			ensure!(
				ledger.unlocking.len() < MAX_UNLOCKING_CHUNKS,
				"can not schedule more unlock chunks"
			);

		    match value {
		        StakingBalance::Ring(r) => {
		            if is_regular {
		                let now = <timestamp::Module<T>>::now();
		                let mut total_changed: RingBalanceOf<T> = Zero::zero();
		                let mut ring_value_left = r;
                        /// for regular_ring, transform into normal one
                        let regular_items = ledger.regular_items.clone();
                        let new_regular_items = regular_items.into_iter()
                            .filter_map(|mut item| if item.expire_time > now {
                                Some(item)
                            } else {
                            // NOTE: value that a user wants to unbond must
                            // be big enough to unlock all regular_ring
                                let res = if ring_value_left.is_zero() {
                                    None
                                } else {
                                    let value = ring_value_left.min(item.value);
                                    ring_value_left = r.saturating_sub(value);

                                    ledger.regular_ring = ledger.regular_ring.saturating_sub(value);
                                    ledger.normal_ring = ledger.normal_ring.saturating_add(value);
                                    ledger.active_ring = ledger.active_ring.saturating_sub(value);
                                    total_changed += value;
                                    item.value -= value;

                                    let res = if item.value.is_zero() {
                                        None
                                    } else {
                                        Some(item)
                                    };
                                    res
                                };
                                res
                            }).collect::<Vec<_>>();
                        // reduce active power then
                        let dt_power = (total_changed / 10000.into()).saturated_into::<PowerBalance>();
                        let dt_power = dt_power.min(ledger.active_power);
                        ledger.active_power -= dt_power;
                        ledger.regular_items = new_regular_items;
                        // update unlocking list
                        let era = Self::current_era() + T::BondingDuration::get();
				        ledger.unlocking.push(UnlockChunk { value: StakingBalance::Ring(total_changed), era, dt_power });
		            } else {
		                // for normal_ring unbond
		                let value = r.min(ledger.normal_ring);

		                let dt_power = (value / 10000.into()).saturated_into::<PowerBalance>();
                        let dt_power = dt_power.min(ledger.active_power);
                        ledger.active_power -= dt_power;

		                let era = Self::current_era() + T::BondingDuration::get();
				        ledger.unlocking.push(UnlockChunk { value: StakingBalance::Ring(value), era, dt_power });
		            }
		        },

		        StakingBalance::Kton(k) => {
                    let value = k.min(ledger.normal_kton);

                    // update active power
                    let dt_power = value.saturated_into::<PowerBalance>();
                    let dt_power = dt_power.min(ledger.active_power);
                    ledger.active_power -= dt_power;
                    let era = Self::current_era() + T::BondingDuration::get();
				    ledger.unlocking.push(UnlockChunk { value: StakingBalance::Kton(value), era, dt_power });

		        },
		    }
        }


        /// may both withdraw ring and kton at the same time
        fn withdraw_unbonded(origin) {
            let controller = ensure_signed(origin)?;
            let ledger = Self::ledger(&controller).ok_or("not a controller")?;
            let (ledger, id) = ledger.consolidate_unlocked(Self::current_era());
            if id == 1 {
                Self::update_ledger(&controller, &ledger, StakingBalance::Ring(0.into()));
            } else if id == 2 {
                Self::update_ledger(&controller, &ledger, StakingBalance::Kton(0.into()));
            } else if id == 3 {
                Self::update_ledger(&controller, &ledger, StakingBalance::Ring(0.into()));
                Self::update_ledger(&controller, &ledger, StakingBalance::Kton(0.into()));
            }
        }

        fn validate(origin, name: Vec<u8>, ratio: u32, unstake_threshold: u32) {
			let controller = ensure_signed(origin)?;
			let ledger = Self::ledger(&controller).ok_or("not a controller")?;
			let stash = &ledger.stash;
			ensure!(
				unstake_threshold <= MAX_UNSTAKE_THRESHOLD,
				"unstake threshold too large"
			);
            // at most 100%
            let ratio = Perbill::from_percent(ratio.min(100));
            // TODO: 10**9 represent for COIN unit. ugly hacking.
            // FIXME: https://github.com/darwinia-network/darwinia/issues/60
            let payment_ratio = ratio * 1_000_000_000.saturated_into::<RingBalanceOf<T>>();

            let prefs = ValidatorPrefs {unstake_threshold: unstake_threshold, validator_payment_ratio: payment_ratio };

			<Nominators<T>>::remove(stash);
			<Validators<T>>::insert(stash, prefs);
			if !<NodeName<T>>::exists(stash) {
			    <NodeName<T>>::insert(stash, name);
			}
		}

		fn nominate(origin, targets: Vec<<T::Lookup as StaticLookup>::Source>) {
			let controller = ensure_signed(origin)?;
			let ledger = Self::ledger(&controller).ok_or("not a controller")?;
			let stash = &ledger.stash;
			ensure!(!targets.is_empty(), "targets cannot be empty");
			let targets = targets.into_iter()
				.take(MAX_NOMINATIONS)
				.map(T::Lookup::lookup)
				.collect::<result::Result<Vec<T::AccountId>, &'static str>>()?;

			<Validators<T>>::remove(stash);
			<Nominators<T>>::insert(stash, targets);
		}

		fn chill(origin) {
			let controller = ensure_signed(origin)?;
			let ledger = Self::ledger(&controller).ok_or("not a controller")?;
			let stash = &ledger.stash;
			<Validators<T>>::remove(stash);
			<Nominators<T>>::remove(stash);
		}

		fn set_payee(origin, payee: RewardDestination) {
			let controller = ensure_signed(origin)?;
			let ledger = Self::ledger(&controller).ok_or("not a controller")?;
			let stash = &ledger.stash;
			<Payee<T>>::insert(stash, payee);
		}

		fn set_controller(origin, controller: <T::Lookup as StaticLookup>::Source) {
			let stash = ensure_signed(origin)?;
			let old_controller = Self::bonded(&stash).ok_or("not a stash")?;
			let controller = T::Lookup::lookup(controller)?;
			if <Ledger<T>>::exists(&controller) {
				return Err("controller already paired")
			}
			if controller != old_controller {
				<Bonded<T>>::insert(&stash, &controller);
				if let Some(l) = <Ledger<T>>::take(&old_controller) {
					<Ledger<T>>::insert(&controller, l);
				}
			}
		}

		/// The ideal number of validators.
		fn set_validator_count(#[compact] new: u32) {
			ValidatorCount::put(new);
		}

		// ----- Root calls.

		fn force_new_era() {
			Self::apply_force_new_era()
		}

		/// Set the offline slash grace period.
		fn set_offline_slash_grace(#[compact] new: u32) {
			OfflineSlashGrace::put(new);
		}

		/// Set the validators who cannot be slashed (if any).
		fn set_invulnerables(validators: Vec<T::AccountId>) {
			<Invulnerables<T>>::put(validators);
		}
    }
}


impl<T: Trait> Module<T> {

    pub fn slashable_balance(who: &T::AccountId) -> PowerBalance {
        Self::stakers(who).total
    }

    fn bond_helper_in_ring(
        stash: T::AccountId,
        controller: T::AccountId,
        value: RingBalanceOf<T>,
        promise_month: u32,
        mut ledger: StakingLedgers<
            T::AccountId, RingBalanceOf<T>, KtonBalanceOf<T>, StakingBalance<RingBalanceOf<T>, KtonBalanceOf<T>>,
            PowerBalance, T::Moment>,
    ) {

        // if stash promise to a extra-lock
        // there will be extra reward, kton, which
        // can also be use to stake.
        let regular_item = if !promise_month.is_zero() {
            let kton_return = utils::compute_kton_return::<T>(value, promise_month);
            ledger.regular_ring += value;
            ledger.active_ring += value;

            // for now, kton_return is free
            // mint kton
            T::Kton::deposit_creating(&stash, kton_return);
            let const_month_in_seconds = 2592000;
            let expire_time = <timestamp::Module<T>>::now() + (const_month_in_seconds * promise_month).into();
            Some(RegularItem { value, expire_time })
        } else {
            ledger.normal_ring += value;
            ledger.active_ring += value;
            None
        };
        if let Some(r) = regular_item {
            ledger.regular_items.push(r);
        }

        let power = (value / 10000.into()).saturated_into::<PowerBalance>();
        ledger.total_power += power;
        ledger.active_power += power;

        Self::update_ledger(&controller, &ledger, StakingBalance::Ring(value));
    }

    fn bond_helper_in_kton(
        stash: T::AccountId,
        controller: T::AccountId,
        value: KtonBalanceOf<T>,
        mut ledger: StakingLedgers<
            T::AccountId, RingBalanceOf<T>, KtonBalanceOf<T>, StakingBalance<RingBalanceOf<T>, KtonBalanceOf<T>>,
            PowerBalance, T::Moment>,
    ) {
        let power = value.saturated_into::<PowerBalance>();
        ledger.total_power += power;
        ledger.active_power += power;

        ledger.normal_kton += value;
        ledger.active_kton += value;

        Self::update_ledger(&controller, &ledger, StakingBalance::Kton(value));
    }

    fn update_ledger(
        controller: &T::AccountId,
        ledger: &StakingLedgers<T::AccountId, RingBalanceOf<T>, KtonBalanceOf<T>,
            StakingBalance<RingBalanceOf<T>, KtonBalanceOf<T>>, PowerBalance, T::Moment>,
        staking_balance: StakingBalance<RingBalanceOf<T>, KtonBalanceOf<T>>,
    ) {
        match staking_balance {
            StakingBalance::Ring(r) => T::Ring::set_lock(
                STAKING_ID,
                &ledger.stash,
                ledger.normal_ring + ledger.regular_ring,
                T::BlockNumber::max_value(),
                WithdrawReasons::all(),
            ),

            StakingBalance::Kton(k) => T::Kton::set_lock(
                STAKING_ID,
                &ledger.stash,
                ledger.normal_kton,
                T::BlockNumber::max_value(),
                WithdrawReasons::all(),
            ),
        }

        <Ledger<T>>::insert(controller, ledger);
    }


    fn new_session(session_index: SessionIndex) -> Option<Vec<T::AccountId>> {
        // accumulate good session reward
        let reward = Self::current_session_reward();
        <CurrentEraReward<T>>::mutate(|r| *r += reward);

        if ForceNewEra::take() || session_index % T::SessionsPerEra::get() == 0 {
            Self::new_era()
        } else {
            None
        }
    }


    /// The era has changed - enact new staking set.
   ///
   /// NOTE: This always happens immediately before a session change to ensure that new validators
   /// get a chance to set their session keys.
    fn new_era() -> Option<Vec<T::AccountId>> {
        let reward = Self::session_reward() * Self::current_era_total_reward();
        if !reward.is_zero() {
            let validators = Self::current_elected();
            let len = validators.len() as u32; // validators length can never overflow u64
            let len: RingBalanceOf<T> = len.into();
            let block_reward_per_validator = reward / len;
            for v in validators.iter() {
                Self::reward_validator(v, block_reward_per_validator);
            }
            Self::deposit_event(RawEvent::Reward(block_reward_per_validator));

            // TODO: reward to treasury
        }

        // check if ok to change epoch
        if Self::current_era() % T::ErasPerEpoch::get() == 0 {
            Self::new_epoch();
        }
        // Increment current era.
        CurrentEra::mutate(|s| *s += 1);

        // Reassign all Stakers.
//        let (_, maybe_new_validators) = Self::select_validators();

//        maybe_new_validators
        None
    }

    fn new_epoch() {
        <EpochIndex<T>>::put(Self::epoch_index() + One::one());
        if let Ok(next_era_reward) =  utils::compute_current_era_reward::<T>() {
            // TODO: change to CurrentEraReward
            <CurrentEraTotalReward<T>>::put(next_era_reward);
        }
    }


    fn reward_validator(stash: &T::AccountId, reward: RingBalanceOf<T>) {
        let off_the_table = reward.saturating_mul(Self::validators(stash).validator_payment_ratio);
        let reward = reward - off_the_table;
        let mut imbalance = <PositiveImbalanceOf<T>>::zero();
        let validator_cut = if reward.is_zero() {
            Zero::zero()
        } else {
            let exposure = Self::stakers(stash);
            let total = exposure.total.max(One::one());

            for i in &exposure.others {
                let per_u64 = Perbill::from_rational_approximation(i.value, total);
                imbalance.maybe_subsume(Self::make_payout(&i.who, per_u64 * reward));
            }

            let per_u64 = Perbill::from_rational_approximation(exposure.own, total);
            per_u64 * reward
        };
        imbalance.maybe_subsume(Self::make_payout(stash, validator_cut + off_the_table));
        T::Reward::on_unbalanced(imbalance);
    }


    /// Actually make a payment to a staker. This uses the currency's reward function
    /// to pay the right payee for the given staker account.
    fn make_payout(stash: &T::AccountId, amount: RingBalanceOf<T>) -> Option<PositiveImbalanceOf<T>> {
        let dest = Self::payee(stash);
        match dest {
            RewardDestination::Controller => Self::bonded(stash)
                .and_then(|controller|
                    T::Ring::deposit_into_existing(&controller, amount).ok()
                ),
            RewardDestination::Stash =>
                T::Ring::deposit_into_existing(stash, amount).ok(),
        }
    }

    // TODO: ready for hacking
    // not only for slashable ring, but also support kton
    fn slashable_balance_of(stash: &T::AccountId) -> RingBalanceOf<T> {
        Self::bonded(stash).and_then(Self::ledger).map(|l| l.active_ring).unwrap_or_default()
    }

    /// Select a new validator set from the assembled stakers and their role preferences.
    ///
    /// Returns the new `SlotStake` value.
//    fn select_validators() -> (BalanceOf<T>, Option<Vec<T::AccountId>>) {
//        let maybe_elected_set = elect::<T, _, _, _>(
//            Self::validator_count() as usize,
//            Self::minimum_validator_count().max(1) as usize,
//            <Validators<T>>::enumerate(),
//            <Nominators<T>>::enumerate(),
//            Self::slashable_balance_of,
//        );
//
//        if let Some(elected_set) = maybe_elected_set {
//            let mut elected_stashes = elected_set.0;
//            let assignments = elected_set.1;
//
//            // helper closure.
//            let to_balance = |b: ExtendedBalance|
//                <T::CurrencyToVote as Convert<ExtendedBalance, BalanceOf<T>>>::convert(b);
//            let to_votes = |b: BalanceOf<T>|
//                <T::CurrencyToVote as Convert<BalanceOf<T>, u64>>::convert(b) as ExtendedBalance;
//
//            // The return value of this is safe to be converted to u64.
//            // The original balance, `b` is within the scope of u64. It is just extended to u128
//            // to be properly multiplied by a ratio, which will lead to another value
//            // less than u64 for sure. The result can then be safely passed to `to_balance`.
//            // For now the backward convert is used. A simple `TryFrom<u64>` is also safe.
//            let ratio_of = |b, p| (p as ExtendedBalance).saturating_mul(to_votes(b)) / ACCURACY;
//
//            // Compute the actual stake from nominator's ratio.
//            let assignments_with_stakes = assignments.iter().map(|(n, a)| (
//                n.clone(),
//                Self::slashable_balance_of(n),
//                a.iter().map(|(acc, r)| (
//                    acc.clone(),
//                    *r,
//                    to_balance(ratio_of(Self::slashable_balance_of(n), *r)),
//                ))
//                    .collect::<Vec<Assignment<T>>>()
//            )).collect::<Vec<(T::AccountId, BalanceOf<T>, Vec<Assignment<T>>)>>();
//
//            // update elected candidate exposures.
//            let mut exposures = <ExpoMap<T>>::new();
//            elected_stashes
//                .iter()
//                .map(|e| (e, Self::slashable_balance_of(e)))
//                .for_each(|(e, s)| {
//                    let item = Exposure { own: s, total: s, ..Default::default() };
//                    exposures.insert(e.clone(), item);
//                });
//
//            for (n, _, assignment) in &assignments_with_stakes {
//                for (c, _, s) in assignment {
//                    if let Some(expo) = exposures.get_mut(c) {
//                        // NOTE: simple example where this saturates:
//                        // candidate with max_value stake. 1 nominator with max_value stake.
//                        // Nuked. Sadly there is not much that we can do about this.
//                        // See this test: phragmen_should_not_overflow_xxx()
//                        expo.total = expo.total.saturating_add(*s);
//                        expo.others.push(IndividualExposure { who: n.clone(), value: *s });
//                    }
//                }
//            }
//
//            if cfg!(feature = "equalize") {
//                let tolerance = 0_u128;
//                let iterations = 2_usize;
//                let mut assignments_with_votes = assignments_with_stakes.iter()
//                    .map(|a| (
//                        a.0.clone(), a.1,
//                        a.2.iter()
//                            .map(|e| (e.0.clone(), e.1, to_votes(e.2)))
//                            .collect::<Vec<(T::AccountId, ExtendedBalance, ExtendedBalance)>>()
//                    ))
//                    .collect::<Vec<(
//                        T::AccountId,
//                        BalanceOf<T>,
//                        Vec<(T::AccountId, ExtendedBalance, ExtendedBalance)>
//                    )>>();
//                equalize::<T>(&mut assignments_with_votes, &mut exposures, tolerance, iterations);
//            }
//
//            // Clear Stakers and reduce their slash_count.
//            for v in Self::current_elected().iter() {
//                <Stakers<T>>::remove(v);
//                let slash_count = <SlashCount<T>>::take(v);
//                if slash_count > 1 {
//                    <SlashCount<T>>::insert(v, slash_count - 1);
//                }
//            }
//
//            // Populate Stakers and figure out the minimum stake behind a slot.
//            let mut slot_stake = BalanceOf::<T>::max_value();
//            for (c, e) in exposures.iter() {
//                if e.total < slot_stake {
//                    slot_stake = e.total;
//                }
//                <Stakers<T>>::insert(c.clone(), e.clone());
//            }
//
//            // Update slot stake.
//            <SlotStake<T>>::put(&slot_stake);
//
////            for st in <ShouldOffline<T>>::take().iter() {
////                elected_stashes.retain(|ref s| s != &st);
////            }
//
//            // Set the new validator set in sessions.
//            <CurrentElected<T>>::put(&elected_stashes);
//            let validators = elected_stashes.into_iter()
//                .map(|s| Self::bonded(s).unwrap_or_default())
//                .collect::<Vec<_>>();
//            (slot_stake, Some(validators))
//        } else {
//            // There were not enough candidates for even our minimal level of functionality.
//            // This is bad.
//            // We should probably disable all functionality except for block production
//            // and let the chain keep producing blocks until we can decide on a sufficiently
//            // substantial set.
//            // TODO: #2494
//            (Self::slot_stake(), None)
//        }
//    }


    fn apply_force_new_era() {
        ForceNewEra::put(true);
    }

}


impl<T: Trait> OnSessionEnding<T::AccountId> for Module<T> {
    fn on_session_ending(i: SessionIndex) -> Option<Vec<T::AccountId>> {
        Self::new_session(i + 1)
    }
}


impl<T: Trait> OnFreeBalanceZero<T::AccountId> for Module<T> {
    fn on_free_balance_zero(stash: &T::AccountId) {
        if let Some(controller) = <Bonded<T>>::take(stash) {
            <Ledger<T>>::remove(&controller);
        }
        <Payee<T>>::remove(stash);
        <SlashCount<T>>::remove(stash);
        <Validators<T>>::remove(stash);
        <Nominators<T>>::remove(stash);
    }
}



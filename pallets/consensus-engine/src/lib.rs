// This file is part of midnight-node.
// Copyright (C) Midnight Foundation
// SPDX-License-Identifier: Apache-2.0
// Licensed under the Apache License, Version 2.0 (the "License");
// You may not use this file except in compliance with the License.
// You may obtain a copy of the License at
// http://www.apache.org/licenses/LICENSE-2.0
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Pallet driving the consensus-engine change. It must work together with a compatible node.
//!
//! The chain starts on AURA (`Aura`) and progresses through a sequence
//! of states as it arms, schedules, and performs a flip to BABE block
//! production. The [`ConsensusEngineApi`](midnight_primitives_consensus_engine::ConsensusEngineApi)
//! runtime API surfaces which engine is active for a given state.
//!
//! Once governance has armed BABE, the node is expected to start emitting BABE
//! `PreRuntimeDigest`s that signal secondary slots, using the same authority index as computed
//! by the AURA logic. This should be done once the majority of validators have registered their
//! BABE keys.
//!
//! A further governance action is then required to schedule the update. It should be scheduled
//! only after observing that a finalized block contains a BABE `PreRuntimeDigest`. This
//! information is not available in the runtime, so we rely on a manual action here.
//!
//! Once scheduled, the pallet performs the flip at the last block of the epoch.
//! If the last slot of epoch is empty, then migration is postponed to the last block of the epoch.
//! The 'migration' is supposed to initialize pallet-babe state and transits to the final state `Babe`.
//! The first block of the next epoch is authored with BABE.

#![cfg_attr(not(feature = "std"), no_std)]

pub use pallet::*;
pub use weights::WeightInfo;

mod weights;

#[cfg(test)]
mod mock;

#[cfg(test)]
mod tests;

#[frame_support::pallet]
pub mod pallet {
	use crate::WeightInfo;
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;
	use midnight_primitives_consensus_engine::ActiveEngine;
	use sp_consensus_aura::AURA_ENGINE_ID;
	use sp_consensus_babe::BABE_ENGINE_ID;
	use sp_consensus_slots::Slot;

	const STORAGE_VERSION: StorageVersion = StorageVersion::new(0);

	/// Binding invoked at the consensus flip to bootstrap `pallet-babe`.
	///
	/// The consensus-engine pallet owns *when* the flip happens; the runtime owns
	/// *how* to bootstrap BABE (its storage and the AURA→BABE key bridge).
	pub trait BabeMigration {
		/// Called when BABE is armed, before the node starts emitting BABE
		/// pre-runtime digests. Pre-seeds `pallet-babe` so its genesis self-init
		/// (guarded on `GenesisSlot == 0`) never fires while armed, which would
		/// otherwise deposit a bogus epoch descriptor into a header we can't retract.
		fn on_arm();
		/// Bootstrap BABE for its genesis epoch at the flip, given the first slot
		/// of the new epoch.
		fn migrate(babe_genesis_slot: Slot);
	}

	impl BabeMigration for () {
		fn on_arm() {}
		fn migrate(_babe_genesis_slot: Slot) {}
	}

	#[pallet::pallet]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// Origin permitted to drive state transitions.
		type GovernanceOrigin: EnsureOrigin<Self::RuntimeOrigin>;

		/// Midnight (sidechain) epochs should be aligned with BABE epochs,
		/// so they require the same lenght. The flip is performed at an epoch boundary so they stay aligned.
		#[pallet::constant]
		type EpochDuration: Get<u64>;

		/// Initializes `pallet-babe` storage when the flip fires. Bound in the
		/// runtime; `()` for runtimes/tests that do not run BABE.
		type BabeMigration: BabeMigration;

		/// Weight information for this pallet's extrinsics.
		type WeightInfo: WeightInfo;
	}

	/// The consensus-engine transition state machine.
	#[derive(
		Debug,
		Default,
		Clone,
		Copy,
		PartialEq,
		Eq,
		Encode,
		Decode,
		DecodeWithMemTracking,
		MaxEncodedLen,
		TypeInfo,
	)]
	pub enum State {
		/// AURA block production, the baseline state before any transition is armed.
		#[default]
		Aura,
		/// A flip to BABE has been armed but not yet scheduled. Node is supposed to add PreRuntimeDigest of BABE Secondary Plain slots in this state.
		ArmedBabe,
		/// The flip to BABE is armed to take effect at the last block of an epoch.
		/// Blocks are still produced with AURA until the flip actually commits.
		ScheduledFlip,
		/// The post flip state, migration happened, consensus is BABE.
		Babe,
	}

	impl State {
		/// The consensus engine that is active while in this state.
		pub fn active_engine(&self) -> ActiveEngine {
			match self {
				State::Aura | State::ArmedBabe | State::ScheduledFlip => ActiveEngine::Aura,
				State::Babe => ActiveEngine::Babe,
			}
		}
	}

	/// The current consensus-engine transition state.
	#[pallet::storage]
	pub type EngineState<T: Config> = StorageValue<_, State, ValueQuery>;

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		/// Drives the automatic, non-governance part of the state machine each block.
		///
		/// The current slot is read from the AURA pre-runtime digest (the validated,
		/// authoritative slot while AURA is producing).
		fn on_initialize(_n: BlockNumberFor<T>) -> Weight {
			match EngineState::<T>::get() {
				// Before arming, the node must not emit BABE pre-digests. A block
				// carrying one would let `pallet-babe` self-initialize its genesis
				// epoch prematurely (see `BabeMigration::on_arm`), so reject it. This
				// is deterministic — every node reads the same header — so a
				// misbehaving author's block is rejected on import, not just locally.
				State::Aura => {
					assert!(
						!Self::has_babe_pre_digest(),
						"BABE pre-runtime digest present while in state 'Aura'",
					);
				},
				State::ScheduledFlip => {
					if let Some(slot) = Self::current_slot_from_aura_digest()
						&& Self::is_last_slot_of_epoch(slot)
					{
						Self::migrate_to_babe(slot);
						EngineState::<T>::put(State::Babe);
					}
				},
				_ => {},
			}
			T::WeightInfo::on_initialize()
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Arm the flip to BABE: move `Aura` to `ArmedBabe`.
		///
		/// Governance-gated. A no-op unless the engine is currently `Aura`.
		#[pallet::call_index(0)]
		#[pallet::weight(T::WeightInfo::arm_babe())]
		pub fn arm_babe(origin: OriginFor<T>) -> DispatchResult {
			T::GovernanceOrigin::ensure_origin(origin)?;
			if EngineState::<T>::get() == State::Aura {
				// Pre-seed BABE before the node starts emitting BABE pre-digests, so
				// pallet-babe does not prematurely self-initialize its genesis epoch.
				T::BabeMigration::on_arm();
				EngineState::<T>::put(State::ArmedBabe);
			}
			Ok(())
		}

		/// Schedule the flip to BABE: move `ArmedBabe` to `ScheduledFlip`.
		///
		/// Governance-gated. A no-op unless the engine is currently `ArmedBabe`.
		/// The flip itself commits automatically at the next epoch boundary; see
		/// [`Hooks::on_initialize`].
		#[pallet::call_index(1)]
		#[pallet::weight(T::WeightInfo::schedule_flip())]
		pub fn schedule_flip(origin: OriginFor<T>) -> DispatchResult {
			T::GovernanceOrigin::ensure_origin(origin)?;
			if EngineState::<T>::get() == State::ArmedBabe {
				EngineState::<T>::put(State::ScheduledFlip);
			}
			Ok(())
		}
	}

	impl<T: Config> Pallet<T> {
		/// The consensus engine currently active, derived from [`EngineState`].
		pub fn active_engine() -> ActiveEngine {
			EngineState::<T>::get().active_engine()
		}

		/// Hand off to the runtime's BABE bootstrap at the flip and log the transition.
		///
		/// `slot` is the last slot of the ending epoch (the current block's slot).
		/// BABE's genesis is the first slot of the next epoch, so its epoch
		/// boundaries stay aligned with the sidechain epochs.
		fn migrate_to_babe(slot: Slot) {
			let babe_genesis_slot = Self::next_epoch_start(slot);

			T::BabeMigration::migrate(babe_genesis_slot);

			log::info!(
				target: "consensus-engine",
				"Consensus engine flip at the last slot ({:?}) of the epoch; \
				BABE genesis slot {:?}, entering Babe state.",
				slot,
				babe_genesis_slot,
			);
		}

		fn current_slot_from_aura_digest() -> Option<Slot> {
			frame_system::Pallet::<T>::digest().logs.iter().find_map(|log| {
				log.as_pre_runtime().and_then(|(id, mut data)| {
					(id == AURA_ENGINE_ID).then(|| Slot::decode(&mut data).ok()).flatten()
				})
			})
		}

		fn has_babe_pre_digest() -> bool {
			frame_system::Pallet::<T>::digest()
				.logs
				.iter()
				.filter_map(|log| log.as_pre_runtime())
				.any(|(id, _)| id == BABE_ENGINE_ID)
		}

		fn is_last_slot_of_epoch(slot: Slot) -> bool {
			let duration = T::EpochDuration::get().max(1);
			(u64::from(slot) + 1) % duration == 0
		}

		/// The first slot of the epoch after the one containing `slot`.
		fn next_epoch_start(slot: Slot) -> Slot {
			let duration = T::EpochDuration::get().max(1);
			let slot = u64::from(slot);
			Slot::from((slot / duration + 1) * duration)
		}
	}
}

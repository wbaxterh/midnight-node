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

//! Tests for the consensus-engine pallet.

use crate::{State, mock::*, pallet::EngineState};
use frame_support::{assert_noop, assert_ok, traits::Hooks};
use midnight_primitives_consensus_engine::ActiveEngine;
use sp_consensus_slots::Slot;
use sp_runtime::DispatchError;

/// Run the pallet's `on_initialize` hook for the current block.
fn on_initialize() {
	ConsensusEngine::on_initialize(System::block_number());
}

#[test]
fn default_state_is_baseline_aura() {
	new_test_ext().execute_with(|| {
		assert_eq!(EngineState::<Test>::get(), State::Aura);
		assert_eq!(ConsensusEngine::active_engine(), ActiveEngine::Aura);
	});
}

#[test]
fn arm_babe_from_baseline() {
	new_test_ext().execute_with(|| {
		assert!(!BabeArmed::get());
		assert_ok!(ConsensusEngine::arm_babe(RuntimeOrigin::root()));
		assert_eq!(EngineState::<Test>::get(), State::ArmedBabe);
		// Arming pre-seeds BABE so it does not self-initialize prematurely.
		assert!(BabeArmed::get());
		// `ArmedBabe` still authors with AURA.
		assert_eq!(ConsensusEngine::active_engine(), ActiveEngine::Aura);
	});
}

#[test]
#[should_panic(expected = "BABE pre-runtime digest present while in state 'Aura'")]
fn baseline_rejects_blocks_with_babe_pre_digest() {
	new_test_ext().execute_with(|| {
		// Default state is `Aura`; a block carrying a BABE pre-digest is rejected.
		start_block_with_babe_pre_digest(100);
		on_initialize();
	});
}

#[test]
fn babe_pre_digest_is_allowed_once_armed() {
	new_test_ext().execute_with(|| {
		EngineState::<Test>::put(State::ArmedBabe);
		// Once armed the node is expected to emit BABE pre-digests; no rejection.
		start_block_with_babe_pre_digest(100);
		on_initialize();
		assert_eq!(EngineState::<Test>::get(), State::ArmedBabe);
	});
}

#[test]
fn arm_babe_requires_governance_origin() {
	new_test_ext().execute_with(|| {
		assert_noop!(ConsensusEngine::arm_babe(RuntimeOrigin::signed(1)), DispatchError::BadOrigin);
		assert_noop!(ConsensusEngine::arm_babe(RuntimeOrigin::none()), DispatchError::BadOrigin);
		assert_eq!(EngineState::<Test>::get(), State::Aura);
	});
}

#[test]
fn arm_babe_is_rejected_from_other_states() {
	new_test_ext().execute_with(|| {
		for state in [State::ArmedBabe, State::ScheduledFlip, State::Babe] {
			EngineState::<Test>::put(state);
			assert_ok!(ConsensusEngine::arm_babe(RuntimeOrigin::root()));
			assert_eq!(EngineState::<Test>::get(), state);
			// The arm hook only fires on the real Aura -> ArmedBabe transition.
			assert!(!BabeArmed::get());
		}
	});
}

#[test]
fn schedule_flip_from_armed() {
	new_test_ext().execute_with(|| {
		EngineState::<Test>::put(State::ArmedBabe);

		assert_ok!(ConsensusEngine::schedule_flip(RuntimeOrigin::root()));

		assert_eq!(EngineState::<Test>::get(), State::ScheduledFlip);
		// A `ScheduledFlip` still authors with AURA until the flip commits.
		assert_eq!(ConsensusEngine::active_engine(), ActiveEngine::Aura);
	});
}

#[test]
fn schedule_flip_requires_governance_origin() {
	new_test_ext().execute_with(|| {
		EngineState::<Test>::put(State::ArmedBabe);
		assert_noop!(
			ConsensusEngine::schedule_flip(RuntimeOrigin::signed(1)),
			DispatchError::BadOrigin
		);
		assert_noop!(
			ConsensusEngine::schedule_flip(RuntimeOrigin::none()),
			DispatchError::BadOrigin
		);
		assert_eq!(EngineState::<Test>::get(), State::ArmedBabe);
	});
}

#[test]
fn schedule_flip_is_no_op_unless_armed() {
	new_test_ext().execute_with(|| {
		for state in [State::Aura, State::ScheduledFlip, State::Babe] {
			EngineState::<Test>::put(state);
			assert_ok!(ConsensusEngine::schedule_flip(RuntimeOrigin::root()));
			assert_eq!(EngineState::<Test>::get(), state);
		}
	});
}

#[test]
fn flip_commits_at_the_last_slot_of_the_epoch() {
	new_test_ext().execute_with(|| {
		EngineState::<Test>::put(State::ScheduledFlip);

		// A mid-epoch block does not trigger the flip.
		start_block_at_slot(1400);
		on_initialize();
		assert_eq!(EngineState::<Test>::get(), State::ScheduledFlip);

		// The last slot of the epoch (1499 for a 300-slot epoch) commits the flip.
		start_block_at_slot(1499);
		on_initialize();
		assert_eq!(EngineState::<Test>::get(), State::Babe);
		assert_eq!(ConsensusEngine::active_engine(), ActiveEngine::Babe);
	});
}

#[test]
fn flip_migrates_with_the_next_epoch_genesis_slot() {
	new_test_ext().execute_with(|| {
		EngineState::<Test>::put(State::ScheduledFlip);
		assert_eq!(BabeMigrateGenesisSlot::get(), None);

		start_block_at_slot(1499);
		on_initialize();

		assert_eq!(EngineState::<Test>::get(), State::Babe);
		// BABE genesis is the first slot of the next epoch (1500), keeping BABE
		// epochs aligned with the sidechain epochs.
		assert_eq!(BabeMigrateGenesisSlot::get(), Some(Slot::from(1500)));
	});
}

#[test]
fn flip_does_not_run_mid_epoch() {
	new_test_ext().execute_with(|| {
		EngineState::<Test>::put(State::ScheduledFlip);
		start_block_at_slot(1400);
		on_initialize();

		assert_eq!(EngineState::<Test>::get(), State::ScheduledFlip);
		assert_eq!(BabeMigrateGenesisSlot::get(), None);
	});
}

#[test]
fn flip_waits_for_next_epoch_when_the_last_slot_is_skipped() {
	new_test_ext().execute_with(|| {
		EngineState::<Test>::put(State::ScheduledFlip);
		// Don't flip at penultimate slot
		start_block_at_slot(1498);
		on_initialize();
		assert_eq!(EngineState::<Test>::get(), State::ScheduledFlip);
		assert_eq!(BabeMigrateGenesisSlot::get(), None);

		// The last slot of the epoch (1499) produced no block; the first block of
		// the next epoch lands at 1500. The flip must NOT commit — we only flip on
		// a block seen exactly at an epoch's last slot.
		start_block_at_slot(1500);
		on_initialize();
		assert_eq!(EngineState::<Test>::get(), State::ScheduledFlip);
		assert_eq!(BabeMigrateGenesisSlot::get(), None);

		// It commits at the next epoch's last slot (1799), genesis slot 1800.
		start_block_at_slot(1799);
		on_initialize();
		assert_eq!(EngineState::<Test>::get(), State::Babe);
		assert_eq!(BabeMigrateGenesisSlot::get(), Some(Slot::from(1800)));
	});
}

#[test]
fn on_initialize_is_a_no_op_in_stable_states() {
	new_test_ext().execute_with(|| {
		for state in [State::Aura, State::ArmedBabe, State::Babe] {
			EngineState::<Test>::put(state);
			// Even at an epoch's last slot, non-scheduled states never flip.
			start_block_at_slot(1499);
			on_initialize();
			assert_eq!(EngineState::<Test>::get(), state);
		}
	});
}

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

//! Runtime binding for the consensus-engine pallet's `BabeMigration`: alters
//! `pallet-babe` storage as the consensus-engine state machine transitions.

use sp_consensus_slots::Slot;

use super::{BABE_GENESIS_RANDOMNESS, Runtime};

/// Alters pallet-babe storages when consensus-engine state machine makes transitions.
pub struct BabeMigrator;
impl pallet_consensus_engine::BabeMigration for BabeMigrator {
	fn on_arm() {
		// The node starts emitting BABE pre-digests once armed. Pre-seed `GenesisSlot`
		// to a non-zero sentinel so pallet-babe's `initialize` (guarded on
		// `GenesisSlot == 0`) does not self-initialize a genesis epoch and deposit a
		// bogus `NextEpochData` digest.
		pallet_babe::GenesisSlot::<Runtime>::put(Slot::from(u64::MAX));
		log::info!(
			target: "runtime::consensus-engine",
			"BABE armed: pre-seeded pallet-babe GenesisSlot to suppress premature genesis init.",
		);
	}
	fn migrate(babe_genesis_slot: Slot) {
		// `babe_genesis_slot` is the first slot of the new epoch, so BABE's epoch
		// boundaries stay aligned with the sidechain epochs. Epoch index 0.
		pallet_babe::GenesisSlot::<Runtime>::put(babe_genesis_slot);
		pallet_babe::CurrentSlot::<Runtime>::put(babe_genesis_slot);
		pallet_babe::EpochIndex::<Runtime>::put(0);

		// Seed both current and next epoch randomness with the genesis bootstrap
		// value; VRF accumulation replaces `NextRandomness` at the first epoch change.
		pallet_babe::Randomness::<Runtime>::put(BABE_GENESIS_RANDOMNESS);
		pallet_babe::NextRandomness::<Runtime>::put(BABE_GENESIS_RANDOMNESS);

		// BABE authorities must come from real registered BABE session keys, not a
		// copy of the AURA set. Until those are wired in, refuse to complete the
		// flip — this panic aborts the block, so nothing above is committed and the
		// engine stays `ScheduledFlip` until the runtime is upgraded. Setting the
		// authorities (and depositing the epoch-0 `NextEpochData` digest) belongs
		// with that work.
		panic!("Issue #1742 adds BABE keys to the runtime");
	}
}

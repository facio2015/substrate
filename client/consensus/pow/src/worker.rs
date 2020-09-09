// This file is part of Substrate.

// Copyright (C) 2017-2020 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use std::{pin::Pin, time::Duration, collections::HashMap, any::Any, borrow::Cow};
use sc_client_api::ImportNotifications;
use sp_runtime::{DigestItem, traits::Block as BlockT, generic::BlockId};
use sp_consensus::{Proposal, BlockOrigin, BlockImportParams, import_queue::BoxBlockImport};
use futures::{prelude::*, task::{Context, Poll}};
use futures_timer::Delay;
use log::*;

use crate::{INTERMEDIATE_KEY, POW_ENGINE_ID, Seal, PowAlgorithm, PowIntermediate};

pub struct MiningMetadata<H, D> {
	pub best_hash: H,
	pub pre_hash: H,
	pub pre_runtime: Option<Vec<u8>>,
	pub difficulty: D,
}

pub struct MiningBuild<Block: BlockT, Algorithm: PowAlgorithm<Block>, C: sp_api::ProvideRuntimeApi<Block>> {
	pub metadata: MiningMetadata<Block::Hash, Algorithm::Difficulty>,
	pub proposal: Proposal<Block, sp_api::TransactionFor<C, Block>>,
}

pub struct MiningWorker<Block: BlockT, Algorithm: PowAlgorithm<Block>, C: sp_api::ProvideRuntimeApi<Block>> {
	pub(crate) build: Option<MiningBuild<Block, Algorithm, C>>,
	pub(crate) algorithm: Algorithm,
	pub(crate) block_import: BoxBlockImport<Block, sp_api::TransactionFor<C, Block>>,
}

impl<Block: BlockT, Algorithm: PowAlgorithm<Block>, C: sp_api::ProvideRuntimeApi<Block>> MiningWorker<Block, Algorithm, C> where
	Algorithm::Difficulty: 'static,
{
	pub fn best_hash(&self) -> Option<Block::Hash> {
		self.build.as_ref().map(|b| b.metadata.best_hash)
	}

	pub(crate) fn on_major_syncing(&mut self) {
		self.build = None;
	}

	pub(crate) fn on_build(
		&mut self,
		build: MiningBuild<Block, Algorithm, C>,
	) {
		self.build = Some(build);
	}

	pub fn submit(&mut self, seal: Seal) -> bool {
		if let Some(build) = self.build.take() {
			match self.algorithm.verify(
				&BlockId::Hash(build.metadata.best_hash),
				&build.metadata.pre_hash,
				build.metadata.pre_runtime.as_ref().map(|v| &v[..]),
				&seal,
				build.metadata.difficulty,
			) {
				Ok(true) => (),
				Ok(false) => {
					warn!(
						target: "pow",
						"Unable to import mined block: seal is invalid",
					);
					return false
				},
				Err(err) => {
					warn!(
						target: "pow",
						"Unable to import mined block: {:?}",
						err,
					);
					return false
				},
			}

			let seal = DigestItem::Seal(POW_ENGINE_ID, seal);
			let (header, body) = build.proposal.block.deconstruct();

			let mut import_block = BlockImportParams::new(BlockOrigin::Own, header);
			import_block.post_digests.push(seal);
			import_block.body = Some(body);
			import_block.storage_changes = Some(build.proposal.storage_changes);

			let intermediate = PowIntermediate::<Algorithm::Difficulty> {
				difficulty: Some(build.metadata.difficulty),
			};

			import_block.intermediates.insert(
				Cow::from(INTERMEDIATE_KEY),
				Box::new(intermediate) as Box<dyn Any>
			);

			match self.block_import.import_block(import_block, HashMap::default()) {
				Ok(_) => {
					info!(
						target: "pow",
						"✅ Successfully mined block on top of: {}",
						build.metadata.best_hash
					);
					true
				},
				Err(err) => {
					warn!(
						target: "pow",
						"Unable to import mined block: {:?}",
						err,
					);
					false
				},
			}
		} else {
			warn!(
				target: "pow",
				"Unable to import mined block: build does not exist",
			);
			false
		}
	}
}

pub struct UntilImportedOrTimeout<Block: BlockT> {
	import_notifications: ImportNotifications<Block>,
	timeout: Duration,
	inner_delay: Option<Delay>,
}

impl<Block: BlockT> UntilImportedOrTimeout<Block> {
	pub fn new(
		import_notifications: ImportNotifications<Block>,
		timeout: Duration,
	) -> Self {
		Self {
			import_notifications,
			timeout,
			inner_delay: None,
		}
	}
}

impl<Block: BlockT> Stream for UntilImportedOrTimeout<Block> {
	type Item = ();

	fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<()>> {
		let mut fire = false;

		loop {
			match Stream::poll_next(Pin::new(&mut self.import_notifications), cx) {
				Poll::Pending => break,
				Poll::Ready(Some(_)) => {
					fire = true;
				},
				Poll::Ready(None) => return Poll::Ready(None),
			}
		}

		self.inner_delay = match self.inner_delay.take() {
			None => {
				Some(Delay::new(self.timeout))
			},
			Some(d) => Some(d),
		};

		if let Some(ref mut inner_delay) = self.inner_delay {
			match Future::poll(Pin::new(inner_delay), cx) {
				Poll::Pending => (),
				Poll::Ready(()) => {
					fire = true;
				},
			}
		}

		if fire {
			self.inner_delay = Some(Delay::new(self.timeout));
			Poll::Ready(Some(()))
		} else {
			Poll::Pending
		}
	}
}
// Copyright 2020 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Implements a `AvailabilityStoreSubsystem`.
//!

#![recursion_limit="256"]

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use codec::{Encode, Decode};
use futures::{select, channel::oneshot, FutureExt};
use kvdb_rocksdb::{Database, DatabaseConfig};
use kvdb::{KeyValueDB, DBTransaction};

use polkadot_primitives::v1::{
	Hash, AvailableData, ErasureChunk, ValidatorIndex,
};
use polkadot_subsystem::{
	FromOverseer, SubsystemError, Subsystem, SubsystemContext, SpawnedSubsystem,
};
use polkadot_subsystem::messages::AvailabilityStoreMessage;

const LOG_TARGET: &str = "availability";

mod columns {
	pub const DATA: u32 = 0;
	pub const NUM_COLUMNS: u32 = 2;
}

#[derive(Debug, derive_more::From)]
enum Error {
	#[from]
	Erasure(erasure::Error),
	#[from]
	Io(io::Error),
	#[from]
	Oneshot(oneshot::Canceled),
	#[from]
	Subsystem(SubsystemError),
}

pub struct AvailabilityStoreSubsystem<Context> {
	inner: Arc<dyn KeyValueDB>,
	_context: std::marker::PhantomData<Context>,
}

fn available_data_key(candidate_hash: &Hash) -> Vec<u8> {
	(candidate_hash, 0i8).encode()
}

fn erasure_chunk_key(candidate_hash: &Hash, index: u32) -> Vec<u8> {
	(candidate_hash, index, 0i8).encode()
}

#[derive(Encode, Decode)]
struct StoredAvailableData {
	data: AvailableData,
	n_validators: u32,
}

impl<Context> AvailabilityStoreSubsystem<Context>
	where
		Context: SubsystemContext<Message=AvailabilityStoreMessage>,
{
	pub fn new(config: Config) -> io::Result<Self> {
		let mut db_config = DatabaseConfig::with_columns(columns::NUM_COLUMNS);

		if let Some(cache_size) = config.cache_size {
			let mut memory_budget = HashMap::new();

			for i in 0..columns::NUM_COLUMNS {
				memory_budget.insert(i, cache_size / columns::NUM_COLUMNS as usize);
			}
			db_config.memory_budget = memory_budget;
		}

		let path = config.path.to_str().ok_or_else(|| io::Error::new(
			io::ErrorKind::Other,
			format!("Bad database path: {:?}", config.path),
		))?;

		let db = Database::open(&db_config, &path)?;

		Ok(Self {
			inner: Arc::new(db),
			_context: std::marker::PhantomData,
		})
	}

	#[cfg(test)]
	fn new_in_memory(inner: Arc<dyn KeyValueDB>) -> Self {
		Self {
			inner,
			_context: std::marker::PhantomData,
		}
	}

	async fn run(mut self, mut ctx: Context) -> Result<(), Error> {
		let ctx = &mut ctx;
		loop {
			select! {
				incoming = ctx.recv().fuse() => {
					match incoming {
						Ok(FromOverseer::Signal(Conclude)) => break,
						Ok(FromOverseer::Signal(_)) => (),
						Ok(FromOverseer::Communication { msg }) => {
							let _ = self.process_message(msg);
						}
						Err(_) => break,
					}
				}
				complete => break,
			}
		}

		Ok(())
	}

	fn process_message(&mut self, msg: AvailabilityStoreMessage) -> Result<(), Error> {
		use AvailabilityStoreMessage::*;
		match msg {
			QueryAvailableData(hash, tx) => {
				let _ = tx.send(self.available_data(&hash).map(|d| d.data));
			}
			QueryChunk(hash, id, tx) => {
				let _ = tx.send(self.get_chunk(&hash, id)?);
			}
			StoreChunk(hash, id, chunk) => {
				self.store_chunk(&hash, id, chunk)?;
			}
			StoreAvailableData(hash, id, n_validators, av_data) => {
				self.store_available_data(&hash, id, n_validators, av_data)?;
			}
		}

		Ok(())
	}

	fn available_data(&self, candidate_hash: &Hash) -> Option<StoredAvailableData> {
		self.query_inner(columns::DATA, &available_data_key(candidate_hash))
	}

	fn store_available_data(
		&self,
		candidate_hash: &Hash,
		id: Option<ValidatorIndex>,
		n_validators: u32,
		available_data: AvailableData,
	) -> Result<(), Error> {
		let mut tx = DBTransaction::new();

		if let Some(index) = id {
			let chunks = get_chunks(&available_data, n_validators as usize)?;
			self.store_chunk(candidate_hash, n_validators, chunks[index as usize].clone())?;
		}

		let stored_data = StoredAvailableData {
			data: available_data,
			n_validators,
		};

		tx.put_vec(
			columns::DATA,
			available_data_key(&candidate_hash).as_slice(),
			stored_data.encode(),
		);

		self.inner.write(tx)?;

		Ok(())
	}

	fn store_chunk(&self, candidate_hash: &Hash, _n_validators: u32, chunk: ErasureChunk)
		-> Result<(), Error>
	{
		let mut tx = DBTransaction::new();

		let dbkey = erasure_chunk_key(candidate_hash, chunk.index);

		tx.put_vec(columns::DATA, &dbkey, chunk.encode());
		self.inner.write(tx)?;

		Ok(())
	}

	fn get_chunk(&self, candidate_hash: &Hash, index: u32) -> Result<Option<ErasureChunk>, Error> {
		if let Some(chunk) = self.query_inner(
			columns::DATA,
			&erasure_chunk_key(candidate_hash, index)) {
			return Ok(Some(chunk));
		}

		if let Some(data) = self.available_data(candidate_hash) {
			let mut chunks = get_chunks(&data.data, data.n_validators as usize)?;
			let chunk = chunks.get(index as usize).cloned();
			for chunk in chunks.drain(..) {
				self.store_chunk(candidate_hash, data.n_validators, chunk)?;
			}
			return Ok(chunk);
		}

		Ok(None)
	}

	fn query_inner<D: Decode>(&self, column: u32, key: &[u8]) -> Option<D> {
		match self.inner.get(column, key) {
			Ok(Some(raw)) => {
				let res = D::decode(&mut &raw[..]).expect("all stored data serialized correctly; qed");
				Some(res)
			}
			Ok(None) => None,
			Err(e) => {
				log::warn!(target: LOG_TARGET, "Error reading from the availability store: {:?}", e);
				None
			}
		}
	}
}

/// Configuration for the availability store.
pub struct Config {
	/// Cache size in bytes. If `None` default is used.
	pub cache_size: Option<usize>,
	/// Path to the database.
	pub path: PathBuf,
}

impl<Context> Subsystem<Context> for AvailabilityStoreSubsystem<Context>
	where
		Context: SubsystemContext<Message=AvailabilityStoreMessage>,
{
	fn start(self, ctx: Context) -> SpawnedSubsystem {
		let future = Box::pin(async move {
			if let Err(e) = self.run(ctx).await {
				log::error!(target: "availabilitystore", "Subsystem exited with an error {:?}", e);
			}
		});

		SpawnedSubsystem {
			future,
			name: "availability-store-subsystem",
		}
	}
}

fn get_chunks(data: &AvailableData, n_validators: usize) -> Result<Vec<ErasureChunk>, Error> {
	let chunks = erasure::obtain_chunks_v1(n_validators, data)?;
	let branches = erasure::branches(chunks.as_ref());

	Ok(chunks
		.iter()
		.zip(branches.map(|(proof, _)| proof))
		.enumerate()
		.map(|(index, (chunk, proof))| ErasureChunk {
			chunk: chunk.clone(),
			proof,
			index: index as u32,
		})
		.collect()
	)
}

#[cfg(test)]
mod tests {
	use super::*;
	use futures::{
		future,
		channel::oneshot,
		executor,
		Future,
	};
	use std::cell::RefCell;
	use polkadot_primitives::v1::{
		AvailableData, BlockData, HeadData, GlobalValidationSchedule, LocalValidationData, PoV,
		OmittedValidationData,
	};

	struct TestHarness {
		virtual_overseer: subsystem_test::TestSubsystemContextHandle<AvailabilityStoreMessage>,
	}

	thread_local! {
		static TIME_NOW: RefCell<Option<u64>> = RefCell::new(None);
	}

	struct TestState {
		global_validation_schedule: GlobalValidationSchedule,
		local_validation_data: LocalValidationData,
	}

	impl Default for TestState {
		fn default() -> Self {

			let local_validation_data = LocalValidationData {
				parent_head: HeadData(vec![7, 8, 9]),
				balance: Default::default(),
				code_upgrade_allowed: None,
				validation_code_hash: Default::default(),
			};

			let global_validation_schedule = GlobalValidationSchedule {
				max_code_size: 1000,
				max_head_data_size: 1000,
				block_number: Default::default(),
			};

			Self {
				local_validation_data,
				global_validation_schedule,
			}
		}
	}

	fn test_harness<T: Future<Output=()>>(
		store: Arc<dyn KeyValueDB>,
		test: impl FnOnce(TestHarness) -> T,
	) {
		let pool = sp_core::testing::SpawnBlockingExecutor::new();

		let (context, virtual_overseer) = subsystem_test::make_subsystem_context(pool.clone());

		let subsystem = AvailabilityStoreSubsystem::new_in_memory(store);
		let subsystem = subsystem.run(context);

		let test_fut = test(TestHarness {
			virtual_overseer,
		});

		futures::pin_mut!(test_fut);
		futures::pin_mut!(subsystem);

		executor::block_on(future::select(test_fut, subsystem));
	}

	#[test]
	fn store_chunk_works() {
		let store = Arc::new(kvdb_memorydb::create(columns::NUM_COLUMNS));
		test_harness(store.clone(), |test_harness| async move {
			let TestHarness { mut virtual_overseer } = test_harness;
			let relay_parent = Hash::from([1; 32]);
			let validator_index = 5;

			let chunk = ErasureChunk {
				chunk: vec![1, 2, 3],
				index: validator_index,
				proof: vec![vec![3, 4, 5]],
			};

			let chunk_msg = AvailabilityStoreMessage::StoreChunk(
				relay_parent,
				validator_index, 
				chunk.clone(),
			);

			virtual_overseer.send(FromOverseer::Communication{ msg: chunk_msg }).await;

			let (tx, rx) = oneshot::channel();
			let query_chunk = AvailabilityStoreMessage::QueryChunk(
				relay_parent,
				validator_index,
				tx,
			);

			virtual_overseer.send(FromOverseer::Communication{ msg: query_chunk }).await;

			assert_eq!(rx.await.unwrap().unwrap(), chunk);
		});
	}

	#[test]
	fn store_block_works() {
		let store = Arc::new(kvdb_memorydb::create(columns::NUM_COLUMNS));
		let test_state = TestState::default();
		test_harness(store.clone(), |test_harness| async move {
			let TestHarness { mut virtual_overseer } = test_harness;
			let candidate_hash = Hash::from([1; 32]);
			let validator_index = 5;
			let n_validators = 10;

			let pov = PoV {
				block_data: BlockData(vec![4, 5, 6]),
			};

			let global_validation = test_state.global_validation_schedule;
			let local_validation = test_state.local_validation_data;

			let omitted_validation = OmittedValidationData {
				global_validation,
				local_validation,
			};
				
			let available_data = AvailableData {
				pov,
				omitted_validation,
			};

			let block_msg = AvailabilityStoreMessage::StoreAvailableData(
				candidate_hash,
				Some(validator_index),
				n_validators,
				available_data.clone(),
			);

			virtual_overseer.send(FromOverseer::Communication{ msg: block_msg }).await;

			let pov = query_available_data(&mut virtual_overseer, candidate_hash).await.unwrap();

			assert_eq!(pov, available_data);

			let chunk = query_chunk(&mut virtual_overseer, candidate_hash, validator_index).await.unwrap();

			let chunks = erasure::obtain_chunks_v1(10, &available_data).unwrap();

			let mut branches = erasure::branches(chunks.as_ref());

			let branch = branches.nth(5).unwrap();
			let expected_chunk = ErasureChunk {
				chunk: branch.1.to_vec(),
				index: 5,
				proof: branch.0,
			};

			assert_eq!(chunk, expected_chunk);
		});
	}


	#[test]
	fn store_pov_and_query_chunk_works() {
		let store = Arc::new(kvdb_memorydb::create(columns::NUM_COLUMNS));
		let test_state = TestState::default();

		test_harness(store.clone(), |test_harness| async move {
			let TestHarness { mut virtual_overseer } = test_harness;
			let candidate_hash = Hash::from([1; 32]);
			let n_validators = 10;

			let pov = PoV {
				block_data: BlockData(vec![4, 5, 6]),
			};

			let global_validation = test_state.global_validation_schedule;
			let local_validation = test_state.local_validation_data;

			let omitted_validation = OmittedValidationData {
				global_validation,
				local_validation,
			};

			let available_data = AvailableData {
				pov,
				omitted_validation,
			};

			let chunks_expected = get_chunks(&available_data, n_validators as usize).unwrap();

			let block_msg = AvailabilityStoreMessage::StoreAvailableData(
				candidate_hash,
				None,
				n_validators,
				available_data,
			);

			virtual_overseer.send(FromOverseer::Communication{ msg: block_msg }).await;

			for validator_index in 0..n_validators {
				let chunk = query_chunk(&mut virtual_overseer, candidate_hash, validator_index).await.unwrap();

				assert_eq!(chunk, chunks_expected[validator_index as usize]);
			}
		});
	}

	async fn query_available_data(
		virtual_overseer: &mut subsystem_test::TestSubsystemContextHandle<AvailabilityStoreMessage>,
		candidate_hash: Hash,
	) -> Option<AvailableData> {
		let (tx, rx) = oneshot::channel();

		let query = AvailabilityStoreMessage::QueryAvailableData(candidate_hash, tx);
		virtual_overseer.send(FromOverseer::Communication{ msg: query }).await;

		rx.await.unwrap()
	}

	async fn query_chunk(
		virtual_overseer: &mut subsystem_test::TestSubsystemContextHandle<AvailabilityStoreMessage>,
		candidate_hash: Hash,
		index: u32,
	) -> Option<ErasureChunk> {
		let (tx, rx) = oneshot::channel();

		let query = AvailabilityStoreMessage::QueryChunk(candidate_hash, index, tx);
		virtual_overseer.send(FromOverseer::Communication{ msg: query }).await;

		rx.await.unwrap()
	}
}
